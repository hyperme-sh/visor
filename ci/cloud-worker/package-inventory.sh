#!/bin/sh
set -eu
export LC_ALL=C
directory=/usr/share/visor-worker
install -d "$directory/licenses"
dpkg-query -W -f='${Package}\t${Version}\t${Architecture}\t${source:Package}\t${source:Version}\t${db:Status-Status}\n' > "$directory/packages.tsv"
tab=$(printf '\t')
while IFS="$tab" read -r package version architecture source source_version state; do
    [ "$state" = installed ] || continue
    if [ "$package" = hypermesh-visor-headless ]; then
        cp /usr/share/doc/hypermesh-visor-headless/*LICENSE "$directory/licenses/"
    elif [ -f "/usr/share/doc/$package/copyright" ]; then
        cp -L "/usr/share/doc/$package/copyright" "$directory/licenses/$package"
    fi
done < "$directory/packages.tsv"
