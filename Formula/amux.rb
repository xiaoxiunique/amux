class Amux < Formula
  desc "Run AI coding agents in per-directory persistent tmux sessions"
  homepage "https://github.com/xiaoxiunique/amux"
  url "https://github.com/xiaoxiunique/amux/archive/refs/tags/v0.3.0.tar.gz"
  sha256 "091edca860f515fd4f99618f9829058d3e0d669bd1d14689f6765060c0a2a417"
  license "MIT"
  depends_on "rust" => :build
  depends_on "tmux"

  def install
    system "cargo", "install", "--locked", "--root", prefix, "--path", "."
  end

  test do
    assert_match "amux", shell_output("#{bin}/amux --version")
  end
end
