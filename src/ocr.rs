//! Frame preprocessing and OCR backends.
//!
//! Two interchangeable engines:
//! - `cli`: invokes the `tesseract` binary per frame via stdin/stdout. No
//!   build-time dependencies; ~50-150ms per call, fine at 1 fps.
//! - `leptess` (cargo feature `leptess-ocr`): in-process libtesseract, run on
//!   a dedicated OS thread so the FFI handle never has to cross await points.

use anyhow::{bail, Context, Result};
use image::{imageops, GrayImage};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;

use crate::config::OcrCfg;

pub const WHITELIST: &str = "0123456789:.";

#[derive(Debug, Clone)]
pub struct PreprocessCfg {
    pub upscale: u32,
    pub threshold: u8,
    pub invert: bool,
}

impl From<&crate::config::TimerCfg> for PreprocessCfg {
    fn from(t: &crate::config::TimerCfg) -> Self {
        Self {
            upscale: t.upscale,
            threshold: t.threshold,
            invert: t.invert,
        }
    }
}

/// Upscale, threshold to pure black/white, and orient as dark-text-on-light,
/// which is what tesseract is happiest with.
pub fn preprocess(gray: &GrayImage, cfg: &PreprocessCfg) -> GrayImage {
    let up = cfg.upscale.max(1);
    let mut img = if up > 1 {
        imageops::resize(
            gray,
            gray.width() * up,
            gray.height() * up,
            imageops::FilterType::CatmullRom,
        )
    } else {
        gray.clone()
    };
    for px in img.pixels_mut() {
        let lit = px.0[0] > cfg.threshold;
        // With invert=true, bright source pixels (digits on a dark LiveSplit
        // background) come out black on white.
        px.0[0] = if lit ^ cfg.invert { 255 } else { 0 };
    }
    img
}

pub fn to_png(img: &GrayImage) -> Result<Vec<u8>> {
    let mut buf = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageLuma8(img.clone())
        .write_to(&mut buf, image::ImageFormat::Png)
        .context("encoding png")?;
    Ok(buf.into_inner())
}

pub enum OcrEngine {
    Cli(CliOcr),
    #[cfg(feature = "leptess-ocr")]
    Leptess(leptess_worker::LeptessWorker),
}

impl OcrEngine {
    pub fn from_config(cfg: &OcrCfg) -> Result<Self> {
        match cfg.engine.as_str() {
            "cli" => Ok(Self::Cli(CliOcr::new(cfg)?)),
            "leptess" => {
                #[cfg(feature = "leptess-ocr")]
                {
                    Ok(Self::Leptess(leptess_worker::LeptessWorker::spawn(cfg)?))
                }
                #[cfg(not(feature = "leptess-ocr"))]
                {
                    bail!(
                        "this binary was built without the `leptess-ocr` feature; \
                         rebuild with `cargo build --features leptess-ocr` or set \
                         ocr.engine = \"cli\""
                    )
                }
            }
            "auto" => {
                #[cfg(feature = "leptess-ocr")]
                {
                    Ok(Self::Leptess(leptess_worker::LeptessWorker::spawn(cfg)?))
                }
                #[cfg(not(feature = "leptess-ocr"))]
                {
                    Ok(Self::Cli(CliOcr::new(cfg)?))
                }
            }
            other => bail!("unknown ocr.engine {other:?}"),
        }
    }

    pub async fn recognize(&mut self, png: &[u8]) -> Result<String> {
        match self {
            Self::Cli(c) => c.recognize(png).await,
            #[cfg(feature = "leptess-ocr")]
            Self::Leptess(l) => l.recognize(png).await,
        }
    }
}

pub struct CliOcr {
    cmd: String,
    lang: String,
}

impl CliOcr {
    pub fn new(cfg: &OcrCfg) -> Result<Self> {
        let out = std::process::Command::new(&cfg.tesseract_cmd)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .with_context(|| {
                format!(
                    "cannot execute {:?} — install tesseract (Gentoo: `emerge app-text/tesseract`) \
                     or build with `--features leptess-ocr`",
                    cfg.tesseract_cmd
                )
            })?;
        if !out.success() {
            bail!("{:?} --version exited with {out}", cfg.tesseract_cmd);
        }
        Ok(Self {
            cmd: cfg.tesseract_cmd.clone(),
            lang: cfg.lang.clone(),
        })
    }

    pub async fn recognize(&self, png: &[u8]) -> Result<String> {
        // A wedged tesseract must not freeze the whole pipeline: bound the
        // call and let kill_on_drop reap the child.
        tokio::time::timeout(std::time::Duration::from_secs(15), self.recognize_inner(png))
            .await
            .map_err(|_| anyhow::anyhow!("tesseract timed out after 15s"))?
    }

    async fn recognize_inner(&self, png: &[u8]) -> Result<String> {
        let mut child = tokio::process::Command::new(&self.cmd)
            .args([
                "stdin",
                "stdout",
                "--dpi",
                "96",
                "--psm",
                "7", // single text line
                "-l",
                &self.lang,
                "-c",
                &format!("tessedit_char_whitelist={WHITELIST}"),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("spawning tesseract")?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin.write_all(png).await?;
        drop(stdin);
        let out = child.wait_with_output().await?;
        if !out.status.success() {
            bail!("tesseract exited with {}", out.status);
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

#[cfg(feature = "leptess-ocr")]
pub mod leptess_worker {
    //! LepTess holds raw FFI pointers, so instead of arguing with `Send`
    //! bounds we give it its own OS thread and talk to it over channels.

    use anyhow::{anyhow, Result};
    use tokio::sync::{mpsc, oneshot};

    use super::WHITELIST;
    use crate::config::OcrCfg;

    type Job = (Vec<u8>, oneshot::Sender<Result<String>>);

    pub struct LeptessWorker {
        tx: mpsc::Sender<Job>,
    }

    impl LeptessWorker {
        pub fn spawn(cfg: &OcrCfg) -> Result<Self> {
            let (tx, mut rx) = mpsc::channel::<Job>(2);
            let datapath = cfg.tessdata_path.clone();
            let lang = cfg.lang.clone();
            let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<()>>();
            std::thread::Builder::new()
                .name("leptess-ocr".into())
                .spawn(move || {
                    let mut lt = match init(datapath.as_deref(), &lang) {
                        Ok(lt) => {
                            let _ = init_tx.send(Ok(()));
                            lt
                        }
                        Err(e) => {
                            let _ = init_tx.send(Err(e));
                            return;
                        }
                    };
                    while let Some((png, reply)) = rx.blocking_recv() {
                        let res = recognize_one(&mut lt, &png);
                        let _ = reply.send(res);
                    }
                })
                .expect("spawning ocr thread");
            init_rx
                .recv()
                .map_err(|_| anyhow!("leptess init thread died"))??;
            Ok(Self { tx })
        }

        pub async fn recognize(&mut self, png: &[u8]) -> Result<String> {
            let (otx, orx) = oneshot::channel();
            self.tx
                .send((png.to_vec(), otx))
                .await
                .map_err(|_| anyhow!("leptess worker is gone"))?;
            orx.await.map_err(|_| anyhow!("leptess worker dropped the job"))?
        }
    }

    fn init(datapath: Option<&str>, lang: &str) -> Result<leptess::LepTess> {
        let mut lt = leptess::LepTess::new(datapath, lang)
            .map_err(|e| anyhow!("initializing tesseract: {e}"))?;
        lt.set_variable(leptess::Variable::TesseditCharWhitelist, WHITELIST)
            .map_err(|e| anyhow!("setting whitelist: {e}"))?;
        lt.set_variable(leptess::Variable::TesseditPagesegMode, "7")
            .map_err(|e| anyhow!("setting psm: {e}"))?;
        Ok(lt)
    }

    fn recognize_one(lt: &mut leptess::LepTess, png: &[u8]) -> Result<String> {
        lt.set_image_from_mem(png)
            .map_err(|e| anyhow!("loading image: {e}"))?;
        lt.get_utf8_text().map_err(|e| anyhow!("ocr: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Luma;

    #[test]
    fn threshold_and_invert() {
        let mut img = GrayImage::new(2, 2);
        img.put_pixel(0, 0, Luma([0]));
        img.put_pixel(1, 0, Luma([255]));
        img.put_pixel(0, 1, Luma([100]));
        img.put_pixel(1, 1, Luma([200]));
        let cfg = PreprocessCfg {
            upscale: 1,
            threshold: 140,
            invert: true,
        };
        let out = preprocess(&img, &cfg);
        // Bright pixels (digits) -> black; dark background -> white.
        assert_eq!(out.get_pixel(0, 0).0[0], 255);
        assert_eq!(out.get_pixel(1, 0).0[0], 0);
        assert_eq!(out.get_pixel(0, 1).0[0], 255);
        assert_eq!(out.get_pixel(1, 1).0[0], 0);

        let cfg = PreprocessCfg {
            invert: false,
            ..cfg
        };
        let out = preprocess(&img, &cfg);
        assert_eq!(out.get_pixel(0, 0).0[0], 0);
        assert_eq!(out.get_pixel(1, 0).0[0], 255);
    }

    #[test]
    fn upscale_multiplies_dimensions() {
        let img = GrayImage::new(10, 4);
        let cfg = PreprocessCfg {
            upscale: 4,
            threshold: 128,
            invert: true,
        };
        let out = preprocess(&img, &cfg);
        assert_eq!((out.width(), out.height()), (40, 16));
    }

    #[test]
    fn png_roundtrip() {
        let img = GrayImage::new(8, 8);
        let png = to_png(&img).unwrap();
        let back = image::load_from_memory(&png).unwrap();
        assert_eq!((back.width(), back.height()), (8, 8));
    }
}
