# Homebrew cask template for the menu bar app.
#
# DO NOT SUBMIT to homebrew/homebrew-cask until:
#   1. The app is signed + notarized (`app/notarize.sh` must succeed).
#   2. A signed zip is uploaded as a GitHub release asset.
#   3. The `url` below points at that asset and `sha256` matches.
#
# Then: fork homebrew/homebrew-cask, drop this at Casks/p/power-monitor.rb,
# open a PR. Install becomes `brew install --cask power-monitor`.

cask "power-monitor" do
  version "0.1.0"
  # Replace :no_check with the sha256 printed by `app/notarize.sh`.
  sha256 :no_check

  # TODO point at the signed GitHub release asset:
  # url "https://github.com/electricapp/power-monitor/releases/download/v#{version}/PowerMonitorMenuBar-v#{version}-macos-arm64.zip"
  url "about:blank"

  name "Power Monitor"
  desc "Apple Silicon power, performance, and thermal menu bar monitor"
  # homepage "https://github.com/electricapp/power-monitor"

  depends_on arch: :arm64
  depends_on macos: ">= :ventura"

  app "PowerMonitorMenuBar.app"

  zap trash: [
    "~/Library/LaunchAgents/com.power-monitor*.plist",
    "/tmp/power-monitor*.log",
  ]
end
