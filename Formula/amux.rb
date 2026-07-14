class Amux < Formula
  desc "Run AI coding agents in per-directory persistent tmux sessions"
  homepage "https://github.com/xiaoxiunique/amux"
  url "https://github.com/xiaoxiunique/amux/archive/refs/tags/v0.5.0.tar.gz"
  sha256 "f0caff4d7d84ad93e9e37c287f8b6d1c6cec7ecc1af98587310c95d9a463ac6f"
  license "MIT"
  depends_on "rust" => :build
  depends_on "rmux"

  def install
    system "cargo", "install", "--locked", "--root", prefix, "--path", "."
  end

  test do
    assert_match "amux", shell_output("#{bin}/amux --version")
  end
end
