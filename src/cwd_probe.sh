# SaguTerm: descobre a pasta atual do shell interativo deste terminal SSH.
#
# Enviado pelo stdin de um canal exec ("sh -s") aberto na MESMA conexao do
# terminal: o sshd cria o shell interativo (lider de sessao com tty) e este
# script como filhos do mesmo processo, entao o shell e achado pela estrutura
# de processos em /proc. O app troca @NONCE@ por 16 hex aleatorios e so aceita
# as linhas com esse nonce (ruido de .bashrc/motd nunca e confundido).
#
# Saida (exatamente estas duas linhas, nesta ordem):
#   @@SAGUSHD <nonce> <pasta do shell, pode ser vazia>
#   @@SAGUCWD <nonce> 1 <metodo> <w> <motivo> <comm> <host> <pasta>
# metodo: tmux | fg | shell | home | none
# motivo: ok | tmux-failed | multiplexer | fg-unreadable | cwd-unreadable |
#         no-shell | no-proc
# Mantenha este arquivo com fim de linha LF (.gitattributes).
sagu_probe() {
N='@NONCE@'
NL='
'
LC_ALL=C; export LC_ALL
PATH=$PATH:/usr/bin:/bin:/usr/sbin:/sbin
H=$(uname -n 2>/dev/null); H=$(printf '%s' "$H" | tr -c 'A-Za-z0-9._-' '_')
FGC=; SHC=; SHD=

emit() { # method reason comm path
  w=0; [ -n "$4" ] && [ -w "$4" ] && w=1
  c=$(printf '%s' "$3" | tr -c 'A-Za-z0-9._:+@=-' '_')
  printf '@@SAGUSHD %s %s\n' "$N" "$SHD"
  printf '@@SAGUCWD %s 1 %s %s %s %s %s %s\n' "$N" "$1" "$w" "$2" "${c:--}" "${H:--}" "$4"
  exit 0
}
okdir() {
  case $1 in /*) ;; *) return 1 ;; esac
  case $1 in *"$NL"*) return 1 ;; esac
  [ -d "$1" ]
}
home() { # reason
  D=$(cd -P -- "$HOME" 2>/dev/null && pwd -P)
  okdir "$D" && emit home "$1" "${FGC:-$SHC}" "$D"
  emit none "$1" "${FGC:-$SHC}" ""
}
cwd_of() { # pid -> D
  D=$(cd -P -- "/proc/$1/cwd" 2>/dev/null && pwd -P) || return 1
  okdir "$D"
}
read_stat() { # pid -> S_COMM S_PPID S_PGRP S_SID S_TTY S_TPGID S_START
  L=
  IFS= read -r L < "/proc/$1/stat" || return 1
  R=${L##*") "}
  [ "$R" = "$L" ] && return 1
  S_COMM=${L#*"("}; S_COMM=${S_COMM%")"*}
  set -- $R
  [ $# -ge 20 ] || return 1
  S_PPID=$2 S_PGRP=$3 S_SID=$4 S_TTY=$5 S_TPGID=$6 S_START=${20}
  case $S_PPID$S_PGRP$S_SID$S_TTY$S_START in *[!0-9]*) return 1 ;; esac
  case $S_TPGID in -1) ;; ''|*[!0-9]*) return 1 ;; esac
  return 0
}
# One pass over /proc/*/stat. mode=cand: session leaders with a ctty whose
# parent is in $2 (ancestor list) -> "level pid start". mode=pgrp: pids whose
# pgrp is $2 -> "pid".
scan() {
  if command -v awk >/dev/null 2>&1; then
    cat /proc/[0-9]*/stat 2>/dev/null | awk -v mode="$1" -v arg="$2" '
      BEGIN { n = split(arg, A, " "); for (i = 1; i <= n; i++) lvl[A[i]] = i - 1 }
      {
        p = $1
        if (p !~ /^[0-9]+$/) next
        r = $0; ok = 0
        while ((j = index(r, ") ")) > 0) { r = substr(r, j + 2); ok = 1 }
        if (!ok) next
        split(r, F, " ")
        if (mode == "pgrp") { if (F[3] == arg) print p; next }
        if (mode == "kids") { if (F[2] == arg && F[3] == arg) print F[20], p; next }
        if (F[5] == 0 || F[4] != p || !(F[2] in lvl)) next
        print lvl[F[2]], p, F[20]
      }'
    return
  fi
  for f in /proc/[0-9]*/stat; do
    p=${f#/proc/}; p=${p%/stat}
    read_stat "$p" || continue
    if [ "$1" = pgrp ]; then [ "$S_PGRP" = "$2" ] && echo "$p"; continue; fi
    if [ "$1" = kids ]; then
      [ "$S_PPID" = "$2" ] && [ "$S_PGRP" = "$2" ] && echo "$S_START $p"
      continue
    fi
    [ "$S_TTY" = 0 ] && continue
    [ "$S_SID" = "$p" ] || continue
    l=0
    for x in $2; do
      [ "$x" = "$S_PPID" ] && { echo "$l $p $S_START"; break; }
      l=$((l+1))
    done
  done
}
tmux_cwd() { # client pid -> D
  tp=$1
  b=$(readlink "/proc/$tp/exe" 2>/dev/null)
  case $b in /*) [ -x "$b" ] || b=tmux ;; *) b=tmux ;; esac
  o=$(tr '\000' '\n' < "/proc/$tp/cmdline" 2>/dev/null | {
    IFS= read -r a || exit 0
    p=
    while IFS= read -r a; do
      case $p in -L|-S) printf '%s%s' "$p" "$a"; exit 0 ;; -c|-f|-T) p=; continue ;; esac
      case $a in -L?*|-S?*) printf '%s' "$a"; exit 0 ;; -L|-S|-c|-f|-T) p=$a ;; -*) p= ;; *) exit 0 ;; esac
    done
  })
  ct=$(readlink "/proc/$tp/fd/0" 2>/dev/null)
  if [ -n "$o" ]; then set -- "$o"; else set --; fi
  T=; command -v timeout >/dev/null 2>&1 && T='timeout 3'
  out=$($T "$b" "$@" list-clients -F '#{client_pid}|#{client_tty}|#{pane_current_path}' 2>/dev/null) || return 1
  D=
  while IFS= read -r ln; do
    lp=${ln%%"|"*}; r=${ln#*"|"}; lt=${r%%"|"*}
    if [ "$lp" = "$tp" ] || { [ -n "$ct" ] && [ "$lt" = "$ct" ]; }; then D=${r#*"|"}; break; fi
  done <<EOF
$out
EOF
  okdir "$D"
}
# Sem /proc (BSD, macOS...): nao da para achar o shell; o app pergunta.
[ -r "/proc/$$/stat" ] || home no-proc

# 1. Ancestors of this exec shell, closest first; stop at the ssh daemon.
ANC= a=$PPID n=0
while [ -n "$a" ] && [ "$a" -gt 1 ] && [ $n -lt 4 ]; do
  ANC="$ANC $a"; n=$((n+1))
  read_stat "$a" || break
  case $S_COMM in sshd*|dropbear*) break ;; esac
  a=$S_PPID
done

# 2. Interactive PTY shell = session leader with a ctty whose parent is the
#    closest ancestor; prefer same SSH_CONNECTION, then newest.
SH= SHL=99 SHM=0 SHS=0
while read -r l p st; do
  [ -n "$p" ] || continue
  m=0
  if [ -n "$SSH_CONNECTION" ] && [ -r "/proc/$p/environ" ]; then
    tr '\000' '\n' < "/proc/$p/environ" 2>/dev/null | grep -qxF "SSH_CONNECTION=$SSH_CONNECTION" && m=1
  fi
  if [ "$l" -lt "$SHL" ] ||
     { [ "$l" -eq "$SHL" ] && { [ "$m" -gt "$SHM" ] ||
       { [ "$m" -eq "$SHM" ] && [ "$st" -gt "$SHS" ]; }; }; }; then
    SH=$p SHL=$l SHM=$m SHS=$st
  fi
done <<EOF
$(scan cand "$ANC")
EOF

[ -n "$SH" ] || home no-shell
read_stat "$SH" || home no-shell
SHC=$S_COMM TP=$S_TPGID
SHD=; cwd_of "$SH" && SHD=$D

# 2b. Sem controle de jobs (ex.: programa aberto pelo .bashrc, como um tmux
#     automatico sem "exec"), o filho em primeiro plano fica no grupo do
#     proprio shell e o tty aponta o shell, como se ele estivesse no prompt.
#     Nesse caso o filho mais novo no grupo do shell e quem esta de fato em
#     primeiro plano.
if [ "$TP" = "$SH" ]; then
  K=$(scan kids "$SH" | sort -n | tail -n 1)
  [ -n "$K" ] && TP=${K#* }
fi

# 3. Foreground job of that tty (if not the shell itself).
if [ "$TP" -gt 0 ] && [ "$TP" != "$SH" ]; then
  FG=
  if read_stat "$TP"; then FGC=$S_COMM; cwd_of "$TP" && FG=$D; fi
  if [ -z "$FG" ]; then
    for p in $(scan pgrp "$TP"); do
      [ "$p" = "$TP" ] && continue
      read_stat "$p" || continue
      [ -n "$FGC" ] || FGC=$S_COMM
      if cwd_of "$p"; then FG=$D; FGC=$S_COMM; break; fi
    done
  fi
  R=ok
  case $FGC in
    tmux*) tmux_cwd "$TP" && emit tmux ok "$FGC" "$D"; R=tmux-failed ;;
    screen*|SCREEN*|zellij*|byobu*) R=multiplexer ;;
  esac
  [ -n "$FG" ] && emit fg "$R" "$FGC" "$FG"
  [ -n "$SHD" ] && emit shell fg-unreadable "$FGC" "$SHD"
  home cwd-unreadable
fi
# 4. Shell no prompt. Se o "shell" e um multiplexador (ex.: "exec tmux" no
#    .bashrc), a pasta dele esta velha: pergunta ao tmux ou marca multiplexer.
case $SHC in
  tmux*)
    tmux_cwd "$SH" && emit tmux ok "$SHC" "$D"
    [ -n "$SHD" ] && emit shell multiplexer "$SHC" "$SHD"
    home cwd-unreadable ;;
  screen*|SCREEN*|zellij*|byobu*)
    [ -n "$SHD" ] && emit shell multiplexer "$SHC" "$SHD"
    home cwd-unreadable ;;
esac
[ -n "$SHD" ] && emit shell ok "$SHC" "$SHD"
home cwd-unreadable
}
sagu_probe </dev/null 2>/dev/null
