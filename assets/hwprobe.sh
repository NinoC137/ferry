#!/bin/sh
# hwprobe: a read-only hardware inventory for embedded Linux and Android.
# It deliberately does not load modules, actively enumerate I2C/SPI, alter
# SELinux, or write to sysfs. Optional output files are the only writes.

set -u

VERSION=1.0.0
MAX_DT_NODES=512
MAX_TEXT_BYTES=24576
OUT=
BUNDLE=0
INCLUDE_IDENTIFIERS=0
PROC_ROOT=${HWPROBE_PROC_ROOT:-/proc}
SYS_ROOT=${HWPROBE_SYS_ROOT:-/sys}
DT_ROOT=${HWPROBE_DT_ROOT:-}
WORK_DIR=

usage() {
    cat <<'EOF'
usage: hwprobe.sh collect [options]
       hwprobe.sh doctor [options]

Read-only hardware inventory for embedded Linux and Android.

  --out FILE                 write a JSON report atomically to FILE
  --bundle DIR               additionally save DIR/device-tree.tar
  --include-identifiers      include serial numbers and interface MACs
  --max-dt-nodes N           decode at most N device-tree nodes (default 512)
  --proc-root DIR            alternate procfs root (tests/offline analysis)
  --sys-root DIR             alternate sysfs root (tests/offline analysis)
  --dt-root DIR              alternate live device-tree root

Without --out, the report is written to stdout. Identifiers are redacted by
default; the raw device-tree archive should be treated as diagnostic material.
EOF
}

die() { printf '%s\n' "hwprobe: $*" >&2; exit 2; }
have() { command -v "$1" >/dev/null 2>&1; }

# JSON quote without jq/Python. Inputs are emitted one line at a time and all
# data sources below are textual or converted to text before reaching here.
json_q() {
    printf '%s' "$1" | awk '
        BEGIN { printf "\"" }
        {
            if (NR > 1) printf "\\n"
            gsub(/\\/, "\\\\")
            gsub(/"/, "\\\"")
            gsub(/\t/, "\\t")
            gsub(/\r/, "\\r")
            printf "%s", $0
        }
        END { printf "\"" }
    '
}
json_bool() { [ "$1" = 1 ] || [ "$1" = true ] && printf true || printf false; }
base() { p=${1%/}; printf '%s' "${p##*/}"; }

read_text() {
    f=$1; n=${2:-$MAX_TEXT_BYTES}
    [ -r "$f" ] && tr '\000' '\n' < "$f" 2>/dev/null | head -c "$n" 2>/dev/null
}
file_json() {
    [ -r "$1" ] && json_q "$(read_text "$1" "${2:-$MAX_TEXT_BYTES}")" || printf null
}
file_hex_json() {
    if [ -r "$1" ] && have od; then
        v=$(od -An -v -tx1 "$1" 2>/dev/null | tr -d ' \n' | cut -c "1-${2:-256}")
        json_q "$v"
    else
        printf null
    fi
}
nul_strings_json() {
    f=$1; nul_first=1; printf '['
    if [ -r "$f" ]; then
        tr '\000' '\n' < "$f" 2>/dev/null | awk '/^[ -~]+$/' | while IFS= read -r value; do
            [ "$nul_first" -eq 0 ] && printf ','
            json_q "$value"; nul_first=0
        done
    fi
    printf ']'
}
path_exists_json() { [ -e "$1" ] || [ -L "$1" ] && printf true || printf false; }

prepare_dt_root() {
    [ -n "$DT_ROOT" ] && return
    if [ -d "$SYS_ROOT/firmware/devicetree/base" ]; then
        DT_ROOT=$SYS_ROOT/firmware/devicetree/base
    elif [ -d "$PROC_ROOT/device-tree" ]; then
        DT_ROOT=$PROC_ROOT/device-tree
    else
        DT_ROOT=
    fi
}

is_android() {
    have getprop && { [ -d /system ] || [ -d /apex ] || [ -n "$(getprop ro.build.version.sdk 2>/dev/null)" ]; }
}
getprop_json() { have getprop && json_q "$(getprop "$1" 2>/dev/null)" || printf null; }
emit_android_properties() {
    printf '{"sdk":'; getprop_json ro.build.version.sdk
    printf ',"release":'; getprop_json ro.build.version.release
    printf ',"board":'; getprop_json ro.product.board
    printf ',"device":'; getprop_json ro.product.device
    printf ',"manufacturer":'; getprop_json ro.product.manufacturer
    printf ',"model":'; getprop_json ro.product.model
    printf ',"hardware":'; getprop_json ro.hardware
    printf ',"platform":'; getprop_json ro.board.platform
    printf ',"soc_manufacturer":'; getprop_json ro.soc.manufacturer
    printf ',"soc_model":'; getprop_json ro.soc.model
    printf ',"build_fingerprint":'; getprop_json ro.build.fingerprint
    printf '}'
}

emit_cpu_topology() {
    root=$SYS_ROOT/devices/system/cpu; first=1; printf '['
    for d in "$root"/cpu[0-9]*; do
        [ -d "$d" ] || continue
        [ "$first" -eq 0 ] && printf ','; first=0
        printf '{"cpu":'; json_q "$(base "$d")"
        printf ',"online":'; file_json "$d/online" 32
        printf ',"package_id":'; file_json "$d/topology/physical_package_id" 32
        printf ',"core_id":'; file_json "$d/topology/core_id" 32
        printf ',"cluster_id":'; file_json "$d/topology/cluster_id" 32
        printf ',"max_frequency_khz":'; file_json "$d/cpufreq/cpuinfo_max_freq" 32
        printf '}'
    done
    printf ']'
}

emit_block_devices() {
    root=$SYS_ROOT/class/block; first=1; printf '['
    for d in "$root"/*; do
        [ -e "$d" ] || [ -L "$d" ] || continue
        [ "$first" -eq 0 ] && printf ','; first=0
        printf '{"name":'; json_q "$(base "$d")"
        printf ',"sectors_512":'; file_json "$d/size" 32
        printf ',"logical_block_size":'; file_json "$d/queue/logical_block_size" 32
        printf ',"model":'; file_json "$d/device/model" 256
        printf ',"vendor":'; file_json "$d/device/vendor" 256
        printf ',"removable":'; file_json "$d/removable" 32
        printf '}'
    done
    printf ']'
}

emit_network_interfaces() {
    root=$SYS_ROOT/class/net; first=1; printf '['
    for d in "$root"/*; do
        [ -e "$d" ] || [ -L "$d" ] || continue
        [ "$first" -eq 0 ] && printf ','; first=0
        printf '{"name":'; json_q "$(base "$d")"
        printf ',"type":'; file_json "$d/type" 32
        printf ',"operstate":'; file_json "$d/operstate" 32
        printf ',"mtu":'; file_json "$d/mtu" 32
        printf ',"mac":'
        [ "$INCLUDE_IDENTIFIERS" -eq 1 ] && file_json "$d/address" 64 || printf null
        printf ',"mac_redacted":'; [ "$INCLUDE_IDENTIFIERS" -eq 1 ] && printf false || printf true
        printf '}'
    done
    printf ']'
}

emit_thermal_zones() {
    root=$SYS_ROOT/class/thermal; first=1; printf '['
    for d in "$root"/thermal_zone*; do
        [ -d "$d" ] || continue
        [ "$first" -eq 0 ] && printf ','; first=0
        printf '{"zone":'; json_q "$(base "$d")"
        printf ',"type":'; file_json "$d/type" 256
        printf ',"temperature_millic":'; file_json "$d/temp" 32
        printf '}'
    done
    printf ']'
}

emit_power_supplies() {
    root=$SYS_ROOT/class/power_supply; first=1; printf '['
    for d in "$root"/*; do
        [ -d "$d" ] || continue
        [ "$first" -eq 0 ] && printf ','; first=0
        printf '{"name":'; json_q "$(base "$d")"
        printf ',"type":'; file_json "$d/type" 128
        printf ',"status":'; file_json "$d/status" 128
        printf ',"present":'; file_json "$d/present" 32
        printf ',"capacity_percent":'; file_json "$d/capacity" 32
        printf ',"voltage_now_uv":'; file_json "$d/voltage_now" 32
        printf '}'
    done
    printf ']'
}

emit_bus_devices() {
    root=$1; first=1; printf '['
    for d in "$root"/*; do
        [ -e "$d" ] || [ -L "$d" ] || continue
        [ "$first" -eq 0 ] && printf ','; first=0
        printf '{"id":'; json_q "$(base "$d")"
        printf ',"name":'; file_json "$d/name" 256
        printf ',"modalias":'; file_json "$d/modalias" 256
        printf '}'
    done
    printf ']'
}

emit_usb_devices() {
    root=$SYS_ROOT/bus/usb/devices; first=1; printf '['
    for d in "$root"/*; do
        [ -e "$d/idVendor" ] || continue
        [ "$first" -eq 0 ] && printf ','; first=0
        printf '{"id":'; json_q "$(base "$d")"
        printf ',"vendor_id":'; file_json "$d/idVendor" 32
        printf ',"product_id":'; file_json "$d/idProduct" 32
        printf ',"manufacturer":'; file_json "$d/manufacturer" 256
        printf ',"product":'; file_json "$d/product" 256
        printf ',"serial":'
        [ "$INCLUDE_IDENTIFIERS" -eq 1 ] && file_json "$d/serial" 256 || printf null
        printf '}'
    done
    printf ']'
}

emit_dt_node() {
    node=$1; rel=${node#"$DT_ROOT"}; [ -n "$rel" ] || rel=/
    printf '{"path":'; json_q "$rel"
    printf ',"name":'; json_q "$(base "$node")"
    printf ',"compatible":'; nul_strings_json "$node/compatible"
    printf ',"status":'; file_json "$node/status" 128
    printf ',"device_type":'; file_json "$node/device_type" 128
    printf ',"reg_hex":'; file_hex_json "$node/reg" 256
    printf ',"interrupts_hex":'; file_hex_json "$node/interrupts" 256
    printf ',"clock_names":'; nul_strings_json "$node/clock-names"
    printf '}'
}

emit_dt_nodes() {
    nodes_file=$1; DT_NODE_TOTAL=$(wc -l < "$nodes_file" 2>/dev/null | tr -d ' '); DT_NODE_EMITTED=0; dt_first=1
    printf '['
    while IFS= read -r node; do
        [ -n "$node" ] || continue
        [ "$DT_NODE_EMITTED" -lt "$MAX_DT_NODES" ] || break
        [ "$dt_first" -eq 0 ] && printf ','; dt_first=0
        emit_dt_node "$node"; DT_NODE_EMITTED=$((DT_NODE_EMITTED + 1))
    done < "$nodes_file"
    printf ']'
}

make_bundle() {
    BUNDLE_STATUS=not_requested; BUNDLE_PATH=
    [ "$BUNDLE" -eq 1 ] || return
    [ -n "$DT_ROOT" ] || { BUNDLE_STATUS=device_tree_unavailable; return; }
    [ -n "$OUT" ] || { BUNDLE_STATUS=requires_out_file; return; }
    BUNDLE_PATH=$(dirname "$OUT")/device-tree.tar
    if have tar && tar -cf "$BUNDLE_PATH" -C "$DT_ROOT" . 2>/dev/null; then
        BUNDLE_STATUS=created
    else
        rm -f "$BUNDLE_PATH" 2>/dev/null || true
        BUNDLE_PATH=; BUNDLE_STATUS=tar_failed_or_unavailable
    fi
}

emit_report() {
    prepare_dt_root; make_bundle
    scratch=${WORK_DIR:-${TMPDIR:-/tmp}}
    mkdir -p "$scratch" 2>/dev/null || true
    nodes_file=$scratch/hwprobe-dt-nodes-$$.txt
    trap 'rm -f "$nodes_file" 2>/dev/null || true' EXIT HUP INT TERM
    if [ -n "$DT_ROOT" ] && have find; then find "$DT_ROOT" -type d 2>/dev/null | sort > "$nodes_file"; else : > "$nodes_file"; fi
    platform=linux; is_android && platform=android || true
    uid=$(id -u 2>/dev/null || printf '?'); privileged=false; [ "$uid" = 0 ] && privileged=true

    printf '{"schema":"hwprobe/v1","collector":{"name":"hwprobe","version":'; json_q "$VERSION"
    printf ',"uid":'; json_q "$uid"; printf ',"privileged":%s,"identifier_policy":' "$privileged"
    [ "$INCLUDE_IDENTIFIERS" -eq 1 ] && json_q included || json_q redacted
    printf ',"commands":{"find":'; have find && printf true || printf false
    printf ',"od":'; have od && printf true || printf false
    printf ',"tar":'; have tar && printf true || printf false
    printf ',"getprop":'; have getprop && printf true || printf false
    printf '}}'

    printf ',"platform":{"kind":'; json_q "$platform"
    printf ',"kernel_release":'; file_json "$PROC_ROOT/sys/kernel/osrelease" 256
    printf ',"kernel_version":'; file_json "$PROC_ROOT/version" 4096
    printf ',"hostname":'; file_json "$PROC_ROOT/sys/kernel/hostname" 256
    printf ',"cmdline":'; file_json "$PROC_ROOT/cmdline" 4096
    printf ',"bootconfig":'; file_json "$PROC_ROOT/bootconfig" 8192
    printf ',"android_properties":'; [ "$platform" = android ] && emit_android_properties || printf null
    printf '}'

    printf ',"cpu":{"online":'; file_json "$SYS_ROOT/devices/system/cpu/online" 256
    printf ',"possible":'; file_json "$SYS_ROOT/devices/system/cpu/possible" 256
    printf ',"cpuinfo":'; file_json "$PROC_ROOT/cpuinfo" "$MAX_TEXT_BYTES"
    printf ',"topology":'; emit_cpu_topology; printf '}'
    printf ',"memory":{"meminfo":'; file_json "$PROC_ROOT/meminfo" 16384
    printf ',"swaps":'; file_json "$PROC_ROOT/swaps" 4096; printf '}'
    printf ',"storage":{"partitions":'; file_json "$PROC_ROOT/partitions" 16384
    printf ',"mtd":'; file_json "$PROC_ROOT/mtd" 8192
    printf ',"block_devices":'; emit_block_devices; printf '}'
    printf ',"network":{"interfaces":'; emit_network_interfaces; printf '}'
    printf ',"thermal":{"zones":'; emit_thermal_zones; printf '}'
    printf ',"power":{"supplies":'; emit_power_supplies; printf '}'
    printf ',"buses":{"i2c":'; emit_bus_devices "$SYS_ROOT/bus/i2c/devices"
    printf ',"spi":'; emit_bus_devices "$SYS_ROOT/bus/spi/devices"
    printf ',"usb":'; emit_usb_devices; printf '}'
    printf ',"dmi":{"sys_vendor":'; file_json "$SYS_ROOT/class/dmi/id/sys_vendor" 256
    printf ',"product_name":'; file_json "$SYS_ROOT/class/dmi/id/product_name" 256
    printf ',"product_version":'; file_json "$SYS_ROOT/class/dmi/id/product_version" 256
    printf ',"board_name":'; file_json "$SYS_ROOT/class/dmi/id/board_name" 256
    printf ',"product_serial":'; [ "$INCLUDE_IDENTIFIERS" -eq 1 ] && file_json "$SYS_ROOT/class/dmi/id/product_serial" 256 || printf null
    printf '}'
    printf ',"device_tree":{"source":'; [ -n "$DT_ROOT" ] && json_q "$DT_ROOT" || printf null
    printf ',"available":'; [ -n "$DT_ROOT" ] && printf true || printf false
    printf ',"model":'; [ -n "$DT_ROOT" ] && file_json "$DT_ROOT/model" 512 || printf null
    printf ',"compatible":'; [ -n "$DT_ROOT" ] && nul_strings_json "$DT_ROOT/compatible" || printf '[]'
    printf ',"nodes":'; emit_dt_nodes "$nodes_file"
    printf ',"node_count":%s,"emitted_node_count":%s,"truncated":' "${DT_NODE_TOTAL:-0}" "${DT_NODE_EMITTED:-0}"
    [ "${DT_NODE_TOTAL:-0}" -gt "${DT_NODE_EMITTED:-0}" ] 2>/dev/null && printf true || printf false
    printf '}'
    printf ',"artifacts":{"device_tree_tar":{"status":'; json_q "$BUNDLE_STATUS"
    printf ',"path":'; [ -n "$BUNDLE_PATH" ] && json_q "$BUNDLE_PATH" || printf null
    printf '}}}\n'
}

emit_doctor() {
    prepare_dt_root
    printf '{"schema":"hwprobe-doctor/v1","version":'; json_q "$VERSION"
    printf ',"proc_root":{"path":'; json_q "$PROC_ROOT"; printf ',"cpuinfo_readable":'; [ -r "$PROC_ROOT/cpuinfo" ] && printf true || printf false; printf '}'
    printf ',"sys_root":{"path":'; json_q "$SYS_ROOT"; printf ',"present":'; [ -d "$SYS_ROOT" ] && printf true || printf false; printf '}'
    printf ',"device_tree":{"path":'; [ -n "$DT_ROOT" ] && json_q "$DT_ROOT" || printf null
    printf ',"compatible_readable":'; [ -n "$DT_ROOT" ] && [ -r "$DT_ROOT/compatible" ] && printf true || printf false; printf '}'
    printf ',"commands":{"find":'; have find && printf true || printf false
    printf ',"awk":'; have awk && printf true || printf false
    printf ',"od":'; have od && printf true || printf false
    printf ',"tar":'; have tar && printf true || printf false
    printf ',"getprop":'; have getprop && printf true || printf false; printf '}}\n'
}

parse_args() {
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --out) [ "$#" -ge 2 ] || die '--out needs a file'; OUT=$2; shift 2 ;;
            --bundle) [ "$#" -ge 2 ] || die '--bundle needs a directory'; BUNDLE=1; bundle_dir=$2; shift 2 ;;
            --include-identifiers) INCLUDE_IDENTIFIERS=1; shift ;;
            --max-dt-nodes) [ "$#" -ge 2 ] || die '--max-dt-nodes needs a number'; MAX_DT_NODES=$2; shift 2 ;;
            --proc-root) [ "$#" -ge 2 ] || die '--proc-root needs a directory'; PROC_ROOT=$2; shift 2 ;;
            --sys-root) [ "$#" -ge 2 ] || die '--sys-root needs a directory'; SYS_ROOT=$2; shift 2 ;;
            --dt-root) [ "$#" -ge 2 ] || die '--dt-root needs a directory'; DT_ROOT=$2; shift 2 ;;
            --help|-h) usage; exit 0 ;;
            *) die "unknown option: $1" ;;
        esac
    done
    case "$MAX_DT_NODES" in *[!0-9]*|'') die '--max-dt-nodes must be a non-negative integer';; esac
    if [ "$BUNDLE" -eq 1 ]; then [ -n "$OUT" ] || OUT=$bundle_dir/hardware.json; WORK_DIR=$bundle_dir; fi
}

main() {
    cmd=${1:-collect}
    case "$cmd" in
        collect) shift 2>/dev/null || true; parse_args "$@"
            if [ -n "$OUT" ]; then
                out_dir=$(dirname "$OUT"); mkdir -p "$out_dir" || die "cannot create $out_dir"
                tmp=$OUT.tmp.$$; emit_report > "$tmp" || { rm -f "$tmp"; die 'collection failed'; }
                mv "$tmp" "$OUT" || die "cannot publish $OUT"
            else emit_report; fi ;;
        doctor) shift 2>/dev/null || true; parse_args "$@"; emit_doctor ;;
        help|--help|-h) usage ;;
        *) die "unknown command: $cmd" ;;
    esac
}

main "$@"
