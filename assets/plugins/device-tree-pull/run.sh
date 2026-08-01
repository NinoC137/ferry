#!/bin/sh
# This package is intentionally executed by Ferry's native hardware collector.
# Keeping an entrypoint makes the package inspectable and consistent with every
# local Ferry plugin, while the Rust implementation preserves SSH/ADB behavior
# and safe binary artifact recovery.

printf '%s\n' 'device-tree-pull must be run through Ferry (fy plugin run or Ferry Desktop).' >&2
exit 2
