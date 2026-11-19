#!/bin/bash
# Объём кода модулей пробы P0.6 для раздела «Инженерная стоимость» отчёта и правила выбора
# «Вариант для P3.2» (шаг 3: меньше непустых строк модуля вида, при равенстве — A). Источник —
# spikes/probe-appkit-shell/cost.sh (P0.2). Отличия от P0.2:
#   - модуль — каталог src/<модуль>/, считаются все его файлы *.rs;
#   - каждый файл обрезается на своей строке #[cfg(test)] в начале строки, с неё начинается модуль
#     тестов. Атрибут с отступом (тестовый метод внутри impl) файл не обрезает, такой метод входит
#     в счёт;
#   - методы Objective-C — #[unsafe(method(…))] и #[unsafe(method_id(…))].
# Общие модули, которыми пользуются оба вида, идут отдельными строками и в стоимость варианта не
# входят. Это описание объёма, а не порог качества.
#   cost.sh    таблица по модулям, общие модули и строка модулей вида для правила выбора
# Столбцы: непустые строки, классы define_class!, блоки unsafe { … } по вхождениям, unsafe impl,
# unsafe fn и методы Objective-C.
set -euo pipefail

export LC_ALL=en_US.UTF-8
SRC="$(cd "$(dirname "$0")" && pwd)/src"
MODULES="model highlight view_a view_b"
SHARED="scene paint text draft_field blocks"

command -v rg >/dev/null || {
    echo "cost.sh: нужен rg (ripgrep)" >&2
    exit 1
}

# Файлы модуля: каталог src/<модуль>/ или файл src/<модуль>.rs.
files() {
    if [ -d "$SRC/$1" ]; then
        find "$SRC/$1" -type f -name '*.rs' | LC_ALL=C sort
    else
        echo "$SRC/$1.rs"
    fi
}

# Подстановка $(…) в аргументах echo не роняет скрипт, поэтому модули сверяются до счёта.
for module in $MODULES $SHARED; do
    if [ ! -d "$SRC/$module" ] && [ ! -f "$SRC/$module.rs" ]; then
        echo "cost.sh: нет модуля $module в $SRC" >&2
        exit 1
    fi
done

# Текст модуля до модуля тестов каждого файла.
body() {
    local file
    files "$1" | while IFS= read -r file; do
        awk '/^#\[cfg\(test\)\]/ { exit } { print }' "$file"
    done
}

# Число вхождений шаблона rg в тексте модуля до тестов.
matches() {
    body "$1" | { rg -o -- "$2" || true; } | wc -l | tr -d ' '
}

nonblank() {
    body "$1" | awk 'NF { n++ } END { print n + 0 }'
}

# Ширины столбцов в символах; ${#…} считает символы только в локали UTF-8.
WIDTHS=(12 6 14 10 12 10 13)
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
        "$(matches "$module" '#\[unsafe\(method(_id)?\(')"
}

row "модуль" "строк" "define_class!" "unsafe {}" "unsafe impl" "unsafe fn" "методов ObjC"
for module in $MODULES; do
    read -r -a values <<<"$(measure "$module")"
    row "$module" "${values[@]}"
    case "$module" in
        view_a) view_a="${values[0]}" ;;
        view_b) view_b="${values[0]}" ;;
    esac
done
echo "общие модули обоих видов, в стоимость варианта не входят:"
for module in $SHARED; do
    read -r -a values <<<"$(measure "$module")"
    row "$module" "${values[@]}"
done
if [ "$view_a" -lt "$view_b" ]; then
    fewer="меньше у A"
elif [ "$view_a" -gt "$view_b" ]; then
    fewer="меньше у B"
else
    fewer="поровну, по правилу — A"
fi
echo "строк модуля вида: A (view_a) — $view_a, B (view_b) — $view_b, $fewer"
