#!/usr/bin/perl
# Команда со сроком для run.sh и gitlab/up.sh: deadline.pl <секунды> <команда> [аргумент …].
# Команда идёт дочерним процессом в той же группе процессов. По сроку она получает SIGTERM, через
# 5 с — SIGKILL, а код выхода — 124, даже если после SIGTERM команда вышла с кодом 0. Срок через
# alarm перед exec не годится: рантайм Go (docker CLI) перехватывает SIGALRM и не завершается.
# Без истечения срока — код команды, 128 + номер сигнала, если её завершил сигнал, 127 — команда
# не запустилась.
use strict;
use warnings;

my $secs = shift @ARGV;
defined $secs && $secs =~ /^[1-9][0-9]*$/ && @ARGV
    or die "deadline.pl: ждутся срок в секундах (не меньше 1) и команда\n";
my $pid = fork;
defined $pid or die "deadline.pl: fork: $!\n";
if ($pid == 0) {
    exec { $ARGV[0] } @ARGV or exit 127;
}
my $expired = 0;
$SIG{ALRM} = sub {
    if ($expired++) {
        kill 'KILL', $pid;
    } else {
        kill 'TERM', $pid;
        alarm 5;
    }
};
alarm $secs;
waitpid $pid, 0;
my $status = $?;
alarm 0;
exit 124 if $expired;
exit($status & 127 ? 128 + ($status & 127) : $status >> 8);
