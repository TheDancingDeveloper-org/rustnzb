# Homebrew formula for the prebuilt rustnzb release binary.
#
# This file is a template kept in the source repository; CI never publishes it.
# To ship it, copy it into a tap (or homebrew-core) and open the pull request
# yourself. Refresh the version, both URLs and both SHA-256 values with
# `packaging/homebrew/update-formula.sh <tag>`.
#
# Linux only: the release workflow ships Linux tarballs and a Windows
# installer, so there is no macOS artefact to point a `url` at.
class Rustnzb < Formula
  desc "Usenet NZB downloader with a SABnzbd-compatible API and web UI"
  homepage "https://rustnzb.dev"
  url "https://github.com/TheDancingDeveloper-org/rustnzb/releases/download/v1.5.0/rustnzb-v1.5.0-linux-x86_64.tar.gz"
  sha256 "7fecddc1fb61a33bf3d34ab23ec69e9cbb6398992afa826708b4dc7223018f7d"
  license "MIT"
  version "1.5.0"

  depends_on :linux

  # Extraction. `sevenzip` provides `7zz`, which rustnzb prefers and which can
  # read RAR archives. Homebrew has no `unrar` formula (it is a cask), so
  # direct unpack stays off unless the user installs one themselves.
  depends_on "sevenzip"

  on_arm do
    url "https://github.com/TheDancingDeveloper-org/rustnzb/releases/download/v1.5.0/rustnzb-v1.5.0-linux-aarch64.tar.gz"
    sha256 "bb9cf8313c3a18f42ff4e49dcf137784106717bfdc2d8e2a3753757a3b495b53"
  end

  def install
    bin.install "rustnzb-v#{version}/rustnzb"
    pkgshare.install "rustnzb-v#{version}/config.example.toml"
    doc.install "rustnzb-v#{version}/README.md"
  end

  service do
    run [opt_bin/"rustnzb"]
    keep_alive true
    working_dir var
    log_path var/"log/rustnzb.log"
    error_log_path var/"log/rustnzb.log"
  end

  def caveats
    <<~EOS
      rustnzb reads $XDG_CONFIG_HOME/rustnzb/config.toml (default
      ~/.config/rustnzb/config.toml) and creates it on first start. A fully
      commented example is installed at:
        #{opt_pkgshare}/config.example.toml
      The default config expects /downloads and /data; either create those
      directories or edit the config before starting the service.
      The web UI listens on http://localhost:9090.
    EOS
  end

  test do
    assert_match "rustnzb #{version}", shell_output("#{bin}/rustnzb --version")
    system bin/"rustnzb", "--check-config", "--config", "#{opt_pkgshare}/config.example.toml"
  end
end
