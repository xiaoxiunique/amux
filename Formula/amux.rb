class Amux < Formula
  desc "Run AI coding agents in per-directory persistent tmux sessions"
  homepage "https://github.com/xiaoxiunique/amux"
  url "https://github.com/xiaoxiunique/amux/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "b8cb6a4d1c35e62753243dedd8cb2ccf49f8f27800ea9787b58890a93c31c756"
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
