# Контроль клетки G (задача 15): тот же архив, что у probe-update, без снятия карантина. Сценарий
# brew подставляет @SHA256@ и @URL@.
cask "probe-update-control" do
  version "1"
  sha256 "@SHA256@"

  url "@URL@"
  name "probe-update-control"
  desc "Shellio P0.5 probe control bundle"
  homepage "https://example.invalid/shellio-probe"

  app "probe-update.app"
end
