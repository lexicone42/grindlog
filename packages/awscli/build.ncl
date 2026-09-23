let { upstream, Attrs, BuildSpec, Local, Needs, OutputBin, OutputData, Source, Test, .. } = import "minimal.ncl" in
# Registry packages, referenced by name: this spec lives outside the registry.
let base = upstream "base" in
let python = upstream "python" in
let toolchain = upstream "toolchain" in
let uv = upstream "uv" in

# The AWS CLI (v1, pure Python), for the site deploy: `aws s3 cp`,
# `aws cloudformation` and `aws cloudfront create-invalidation` in
# scripts/deploy-site.sh. Not in the public registry, so defined here, the
# way the registry packages httpie and diffoscope: pip installs the sdist and
# its dependency tree (botocore, s3transfer, docutils, jmespath, rsa,
# python-dateutil, pyyaml, colorama and their transitive deps) into this
# package's own site-packages. `needs` is set because pip resolves that tree
# from PyPI at build time.
let version = "1.46.1" in

{
  name = "awscli",
  build_deps = [
    { file = "build.sh" } | Local,
    {
      url = "https://files.pythonhosted.org/packages/source/a/awscli/awscli-%{version}.tar.gz",
      sha256 = "9dab615cc46d16f1f9750e1c9bd37820a24d6964e9381f712b3a304c2b05d248",
      extract = true,
      strip_prefix = "awscli-%{version}",
    } | Source,
    base,
    toolchain,
    uv,
  ],

  needs =
    {
      dns = {},
      internet = {},
    } | Needs,

  runtime_deps = [
    python,
  ],

  cmd = "./build.sh",

  outputs = {
    # The two executables only: pip also drops shell completer scripts and
    # a Windows .cmd beside them, which are not binaries.
    bin = { glob = "usr/bin/aws" } | OutputBin,
    completer = { glob = "usr/bin/aws_completer" } | OutputBin,
    # allow_executable as httpie does: pyyaml and others may ship compiled
    # extensions, which the output-types check otherwise rejects.
    site_packages = { glob = "usr/lib/python*/site-packages/**", allow_executable = true } | OutputData,
  },

  attrs =
    {
      upstream_version = version,
      license_spdx = "Apache-2.0",
      source_provenance = {
        category = 'GithubRepo,
        owner = "aws",
        repo = "aws-cli",
      },
    } | Attrs,

  tests = {
    smoketest =
      {
        class = 'Standalone,
        test_deps = [base],
        cmds = [
          ["/bin/bash", "-c", "/bin/aws --version | grep -q 'aws-cli/1.46.1'"],
          # Loads botocore and the credential chain without network or a pager
          # (`aws help` wants groff).
          ["/bin/bash", "-c", "/bin/aws configure list >/dev/null"],
        ],
      } | Test,
  },
} | BuildSpec
