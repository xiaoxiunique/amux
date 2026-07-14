class Amux < Formula
  desc "Run AI coding agents in per-directory persistent tmux sessions"
  homepage "https://github.com/xiaoxiunique/amux"
  url "https://github.com/xiaoxiunique/amux/archive/refs/tags/v0.5.2.tar.gz"
  sha256 "408cc8a4fba9a54ede7be1ecb98793f6c07d330c177b698fa6e2bdf80591f6ef"
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
