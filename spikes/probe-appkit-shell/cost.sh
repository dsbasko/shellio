#!/bin/bash
# Объём кода компонентов пробы P0.2 для таблицы инженерной стоимости отчёта. Считаются модули
# компонентов без scenario/, fixtures и тестов: каждый файл обрезается на строке #[cfg(test)],
# с которой начинается модуль тестов. Это описание объёма, а не порог качества.
#   cost.sh    таблица по модулям, итог по компонентам и отдельной строкой сборка сцены
# Столбцы: непустые строки, классы define_class!, блоки unsafe { … } по вхождениям, unsafe impl,
# unsafe fn и методы Objective-C (#[unsafe(method(…))]).
set -euo pipefail

export LC_ALL=en_US.UTF-8
SRC="$(cd "$(dirname "$0")" && pwd)/src"
COMPONENTS="chrome split tiles tabs list tree palette menu theme"

command -v rg >/dev/null || {
    echo "cost.sh: нужен rg (ripgrep)" >&2
    exit 1
}

# Текст модуля до модуля тестов.
body() {
    awk '/^#\[cfg\(test\)\]/ { exit } { print }' "$SRC/$1.rs"
}

# Число вхождений шаблона rg в тексте модуля до тестов.
matches() {
    body "$1" | { rg -o -- "$2" || true; } | wc -l | tr -d ' '
}

nonblank() {
    body "$1" | awk 'NF { n++ } END { print n + 0 }'
}

# Ширины столбцов в символах; ${#…} считает символы только в локали UTF-8.
WIDTHS=(10 6 14 10 12 10 13)
row() {
    local index cell line=""
    for index in "${!WIDTHS[@]}"; do
        cell="${@:index+1:1}"
        if [ "$index" -eq 0 ]; then
            line="$cell$(printf '%*s' $((WIDTHS[index] - ${#cell})) '')"
        else
            line="$line $(printf '%*s' $((WIDTHS[index] - ${#cell})) '')$cell"
        fi
    done
    echo "$line"
}

measure() {
    local module="$1"
    echo "$(nonblank "$module")" \
        "$(matches "$module" '^define_class!\(')" \
        "$(matches "$module" 'unsafe \{')" \
        "$(matches "$module" '\bunsafe impl\b')" \
        "$(matches "$module" '\bunsafe fn\b')" \
        "$(matches "$module" '#\[unsafe\(method\(')"
}

row "модуль" "строк" "define_class!" "unsafe {}" "unsafe impl" "unsafe fn" "методов ObjC"
totals=(0 0 0 0 0 0)
for module in $COMPONENTS; do
    read -r -a values <<<"$(measure "$module")"
    row "$module" "${values[@]}"
    for index in "${!values[@]}"; do
        totals[index]=$((totals[index] + values[index]))
    done
done
row "итого" "${totals[@]}"
read -r -a values <<<"$(measure scene)"
row "scene" "${values[@]}"
echo "scene — сборка сцены из компонентов, в итог компонентов не входит"
