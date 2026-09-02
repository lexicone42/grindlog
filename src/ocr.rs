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

/// One word from a tesseract TSV pass, box in image pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub conf: f32,
    pub text: String,
}

/// Parse tesseract's TSV output (level 5 rows are words):
/// `level page block par line word left top width height conf text`.
pub fn parse_tsv(tsv: &str) -> Vec<Word> {
    tsv.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            if f.len() < 12 || f[0] != "5" {
                return None;
            }
            let text = f[11].trim();
            if text.is_empty() {
                return None;
            }
            Some(Word {
                x: f[6].parse().ok()?,
                y: f[7].parse().ok()?,
                w: f[8].parse().ok()?,
                h: f[9].parse().ok()?,
                conf: f[10].parse().unwrap_or(-1.0),
                text: text.to_string(),
            })
        })
        .collect()
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

    /// Sparse-text pass with word boxes (CLI engine only; other engines
    /// report no words).
    pub async fn recognize_words(
        &mut self,
        png: &[u8],
        whitelist: Option<&str>,
    ) -> Result<Vec<Word>> {
        match self {
            Self::Cli(c) => c.recognize_words(png, whitelist).await,
            #[cfg(feature = "leptess-ocr")]
            Self::Leptess(_) => Ok(Vec::new()),
        }
    }

    /// Like `recognize`, plus the bounding box of the recognized text in
    /// image pixels (x, y, w, h) when the engine can report one.
    pub async fn recognize_boxed(
        &mut self,
        png: &[u8],
    ) -> Result<(String, Option<(u32, u32, u32, u32)>)> {
        match self {
            Self::Cli(c) => c.recognize_boxed(png).await,
            #[cfg(feature = "leptess-ocr")]
            Self::Leptess(l) => l.recognize(png).await.map(|t| (t, None)),
        }
    }
}

/// Union of word boxes; the text is the words joined like tesseract's plain
/// output would be.
pub fn words_to_line(words: &[Word]) -> (String, Option<(u32, u32, u32, u32)>) {
    let text = words
        .iter()
        .map(|w| w.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let x = words.iter().map(|w| w.x).min();
    let y = words.iter().map(|w| w.y).min();
    let right = words.iter().map(|w| w.x + w.w).max();
    let bottom = words.iter().map(|w| w.y + w.h).max();
    let bbox = match (x, y, right, bottom) {
        (Some(x), Some(y), Some(r), Some(b)) => Some((x, y, r - x, b - y)),
        _ => None,
    };
    (text, bbox)
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
        tokio::time::timeout(
            std::time::Duration::from_secs(15),
            self.recognize_inner(png, false),
        )
        .await
        .map_err(|_| anyhow::anyhow!("tesseract timed out after 15s"))?
        .map(|out| out.trim().to_string())
    }

    /// Single-line read that also returns where the text sits in the image
    /// (TSV output in the same call — no second pass).
    pub async fn recognize_boxed(
        &self,
        png: &[u8],
    ) -> Result<(String, Option<(u32, u32, u32, u32)>)> {
        let tsv = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            self.recognize_inner(png, true),
        )
        .await
        .map_err(|_| anyhow::anyhow!("tesseract timed out after 15s"))??;
        Ok(words_to_line(&parse_tsv(&tsv)))
    }

    /// Sparse-text pass over a whole image (`--psm 11`, TSV output): every
    /// word tesseract finds, with its bounding box in image pixels. Used by
    /// `locate` to find the LiveSplit pane; `whitelist` restricts glyphs.
    pub async fn recognize_words(&self, png: &[u8], whitelist: Option<&str>) -> Result<Vec<Word>> {
        let mut args: Vec<String> = [
            "stdin", "stdout", "--dpi", "96", "--psm", "11", "-l", &self.lang,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        if let Some(w) = whitelist {
            args.push("-c".into());
            args.push(format!("tessedit_char_whitelist={w}"));
        }
        args.push("tsv".into());
        let mut child = tokio::process::Command::new(&self.cmd)
            .args(&args)
            .env("OMP_THREAD_LIMIT", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("spawning tesseract")?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin.write_all(png).await?;
        drop(stdin);
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            child.wait_with_output(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("tesseract timed out after 120s"))??;
        if !out.status.success() {
            bail!("tesseract exited with {}", out.status);
        }
        Ok(parse_tsv(&String::from_utf8_lossy(&out.stdout)))
    }

    async fn recognize_inner(&self, png: &[u8], tsv: bool) -> Result<String> {
        let mut cmd = tokio::process::Command::new(&self.cmd);
        cmd.args([
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
        ]);
        if tsv {
            cmd.arg("tsv");
        }
        let mut child = cmd
            // One thread per call: on small crops OpenMP's fan-out costs more
            // than it saves, and several workers share the cores anyway.
            .env("OMP_THREAD_LIMIT", "1")
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
            orx.await
                .map_err(|_| anyhow!("leptess worker dropped the job"))?
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

    #[test]
    fn tsv_words_and_line_box() {
        let tsv = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
                   1\t1\t0\t0\t0\t0\t0\t0\t1560\t400\t-1\t\n\
                   4\t1\t1\t1\t1\t0\t600\t80\t900\t300\t-1\t\n\
                   5\t1\t1\t1\t1\t1\t600\t80\t700\t300\t96.5\t1:41\n\
                   5\t1\t1\t1\t1\t2\t1320\t150\t180\t230\t91.0\t.26\n\
                   5\t1\t1\t1\t1\t3\t1500\t80\t10\t300\t0\t\n";
        let words = parse_tsv(tsv);
        assert_eq!(words.len(), 2, "only level-5 rows with text");
        assert_eq!(words[0].text, "1:41");
        assert_eq!(
            (words[1].x, words[1].y, words[1].w, words[1].h),
            (1320, 150, 180, 230)
        );
        let (text, bbox) = words_to_line(&words);
        assert_eq!(text, "1:41 .26");
        assert_eq!(bbox, Some((600, 80, 900, 300)));
        assert_eq!(words_to_line(&[]), (String::new(), None));
    }
}
