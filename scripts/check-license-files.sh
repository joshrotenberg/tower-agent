#!/bin/sh

set -eu

# Cargo does not follow symlinks when packaging, so each publishable crate
# carries its own copy of the license texts. Copies drift silently, and drift
# in a license is not the kind of thing anyone notices by reading a diff.

status=0

for crate in crates/*/; do
    for license in LICENSE-MIT LICENSE-APACHE; do
        if [ ! -f "${crate}${license}" ]; then
            printf '%s\n' "${crate}${license} is missing"
            status=1
        elif ! cmp -s "$license" "${crate}${license}"; then
            printf '%s\n' "${crate}${license} differs from the workspace copy"
            status=1
        fi
    done
done

if [ "$status" -ne 0 ]; then
    printf '%s\n' "run: for c in crates/*/; do cp LICENSE-MIT LICENSE-APACHE \"\$c\"; done"
    exit 1
fi

printf '%s\n' "license files match the workspace copies"
