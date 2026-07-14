class Amux < Formula
  desc "Run AI coding agents in per-directory persistent tmux sessions"
  homepage "https://github.com/xiaoxiunique/amux"
  url "https://github.com/xiaoxiunique/amux/archive/refs/tags/v0.5.1.tar.gz"
  sha256 "3147edd70f70df395a67af765c79364fd20c8a433bd6bf8cb7c2d5b753c27dfe"
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
