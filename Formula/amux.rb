class Amux < Formula
  desc "Run AI coding agents in per-directory persistent tmux sessions"
  homepage "https://github.com/xiaoxiunique/amux"
  url "https://github.com/xiaoxiunique/amux/archive/refs/tags/v0.2.0.tar.gz"
  sha256 "6d3c113328583c9e825453e04462a3118237dae6ee64cb5d11098fc27d6243ea"
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
