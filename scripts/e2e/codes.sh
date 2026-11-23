#!/usr/bin/env bash
# Коды и итоги сценариев make e2e (ADR-0021, «Уточнено в P1.3 (планирование)» и «Уточнено в P1.4 (планирование)»).
#
#   codes.sh combine <код>…   — код цели по кодам запущенных сценариев: 1 > 3 > 0, любой другой код — 1
#   codes.sh next <код>       — код 0 после кода 0 или 3 предыдущего сценария (следующий запускается), иначе 1
#   codes.sh result <журнал>  — путь result.json из последней строки «итог: код N, result.json — <путь>» журнала
#   codes.sh check <result.json> — разбор result.json сценария: поля, статусы пунктов и код выхода по пунктам;
#                                  нарушения в stderr, код 1
#
# Неверный вызов — код 2. Тесты — crates/shellio-e2e/tests/codes.rs.
set -euo pipefail

usage() {
  echo "usage: $0 combine <код>… | next <код> | result <журнал> | check <result.json>" >&2
  exit 2
}

[[ $# -ge 1 ]] || usage
mode="$1"
shift
case "$mode" in
  combine)
    [[ $# -ge 1 ]] || usage
    code=0
    for c in "$@"; do
      case "$c" in
        0) ;;
        3) [[ $code -eq 1 ]] || code=3 ;;
        *) code=1 ;;
      esac
    done
    echo "$code"
    ;;
  next)
    [[ $# -eq 1 ]] || usage
    case "$1" in
      0 | 3) echo 0 ;;
      *) echo 1 ;;
    esac
    ;;
  result)
    [[ $# -eq 1 && -f "$1" ]] || usage
    sed -n 's/^итог: код [0-9]*, result.json — //p' "$1" | tail -n 1
    ;;
  check)
    [[ $# -eq 1 && -f "$1" ]] || usage
    /usr/bin/perl -COEA -Mutf8 -MJSON::PP -e '
      my $file = shift;
      open my $fh, "<", $file or die "result.json не открывается\n";
      local $/;
      my $r = eval { JSON::PP->new->utf8->decode(<$fh>) };
      if (!$r) { print STDERR "result.json не JSON\n"; exit 1 }
      my @bad;
      my %top = map { $_ => 1 } qw(scenario run_dir bundle exit error items);
      push @bad, "лишнее поле $_" for grep { !$top{$_} } sort keys %$r;
      push @bad, "нет поля $_" for grep { !exists $r->{$_} } sort keys %top;
      push @bad, "scenario не строка" if ref $r->{scenario} || !defined $r->{scenario};
      push @bad, "error не строка и не null" if ref $r->{error};
      my $exit = $r->{exit} // "";
      push @bad, "exit $exit не 0, 1 или 3" unless $exit =~ /^[013]$/;
      my $items = $r->{items};
      if (ref $items ne "ARRAY" || !@$items) { push @bad, "items не список пунктов" ; $items = [] }
      my (%seen, $required_failed);
      my %field = map { $_ => 1 } qw(item required status reason raw);
      for my $i (@$items) {
        if (ref $i ne "HASH") { push @bad, "пункт не объект"; next }
        my $name = $i->{item} // "?";
        push @bad, "$name: лишнее поле $_" for grep { !$field{$_} } sort keys %$i;
        push @bad, "$name: нет поля $_" for grep { !exists $i->{$_} } sort keys %field;
        push @bad, "$name: дважды" if $seen{$name}++;
        push @bad, "$name: required не true или false" unless JSON::PP::is_bool($i->{required});
        my $status = $i->{status} // "";
        push @bad, "$name: статус $status не pass, fail или not_run" unless $status =~ /^(pass|fail|not_run)$/;
        push @bad, "$name: нет причины" unless defined $i->{reason} && length $i->{reason};
        $required_failed = 1 if $i->{required} && $status ne "pass";
      }
      my $want = defined $r->{error} ? 1 : $required_failed ? 3 : 0;
      push @bad, "exit $exit, по пунктам и ошибке исполнения ожидался $want" if $exit =~ /^[013]$/ && $exit != $want;
      print STDERR "$_\n" for @bad;
      exit(@bad ? 1 : 0);
    ' "$1"
    ;;
  *) usage ;;
esac
