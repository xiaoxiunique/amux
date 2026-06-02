class Amux < Formula
  desc "Run AI coding agents in per-directory persistent tmux sessions"
  homepage "https://github.com/<you>/amux"
  url "https://github.com/<you>/amux/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "<fill-after-tagging>"
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
