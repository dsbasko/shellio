# Cask пробы P0.5, клетка G (задача 15). Сценарий brew подставляет @SHA256@ и @URL@ и кладёт файл
# в локальный tap shellio-probe/probe изолированного префикса. Шаги preflight_steps записывают
# xattr -lr staged-копии до и после снятия карантина рядом с каталогом версии в Caskroom.
cask "probe-update" do
  version "1"
  sha256 "@SHA256@"

  url "@URL@"
  name "probe-update"
  desc "Shellio P0.5 probe bundle"
  homepage "https://example.invalid/shellio-probe"

  preflight_steps do
    run "/usr/bin/xattr", args: ["-lr", "{{staged_path}}/probe-update.app"],
                          stdout_path: "{{staged_path}}/../xattr-before.txt"
    run "/usr/bin/xattr", args: ["-dr", "com.apple.quarantine", "{{staged_path}}/probe-update.app"]
    run "/usr/bin/xattr", args: ["-lr", "{{staged_path}}/probe-update.app"],
                          stdout_path: "{{staged_path}}/../xattr-after.txt"
  end

  app "probe-update.app"
end
