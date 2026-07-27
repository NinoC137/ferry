#!/bin/sh
# ferry-gadget.sh — 板端 USB gadget 网络一键脚本（configfs，busybox 兼容）
# 用法: ferry-gadget.sh start|stop|status
#   环境变量可覆盖: MODE=ncm|ecm|rndis  IP=10.55.0.2/30  WITH_ACM=1  SSHD=1
# 由 ferry (fy usb gadget) 生成/推送。

MODE="${MODE:-ncm}"          # ncm: macOS/Linux 主机首选; rndis: 老 Windows 主机
IP="${IP:-10.55.0.2/30}"
WITH_ACM="${WITH_ACM:-1}"    # 顺便暴露一个 USB 串口 console (/dev/ttyGS0)
SSHD="${SSHD:-1}"            # 起来后尝试拉起 dropbear/sshd
G=/sys/kernel/config/usb_gadget/ferry

log() { echo "[ferry-gadget] $*"; }

ensure_configfs() {
    [ -d /sys/kernel/config ] || { log "内核没有 configfs 支持"; return 1; }
    mountpoint -q /sys/kernel/config 2>/dev/null || \
        grep -q configfs /proc/mounts 2>/dev/null || \
        mount -t configfs none /sys/kernel/config || return 1
    modprobe libcomposite 2>/dev/null
    [ -d /sys/kernel/config/usb_gadget ] || { log "usb_gadget configfs 不可用（缺 libcomposite?）"; return 1; }
    return 0
}

mac_from_id() {
    # 由机器标识生成稳定的本地管理 MAC，最后一位区分 host/dev
    seed="$(cat /etc/machine-id 2>/dev/null || cat /proc/cpuinfo 2>/dev/null | grep -i serial | head -1)"
    [ -n "$seed" ] || seed="ferry-$(hostname 2>/dev/null)"
    h="$(echo "$seed" | md5sum 2>/dev/null | cut -c1-8)"
    [ -n "$h" ] || h="00f0e011"
    a=$(echo "$h" | cut -c1-2); b=$(echo "$h" | cut -c3-4)
    c=$(echo "$h" | cut -c5-6); d=$(echo "$h" | cut -c7-8)
    echo "02:$a:$b:$c:$d:$1"
}

fallback_gether() {
    log "configfs 不可用，尝试老内核 g_ether 模块 ..."
    modprobe g_ether 2>/dev/null || { log "g_ether 也不行，放弃"; return 1; }
    sleep 1
    config_ip
}

config_ip() {
    ADDR="${IP%/*}"; BITS="${IP#*/}"
    case "$BITS" in
        30) MASK=255.255.255.252 ;;
        24) MASK=255.255.255.0 ;;
        *)  MASK=255.255.255.252 ;;
    esac
    if command -v ip >/dev/null 2>&1; then
        ip addr flush dev usb0 2>/dev/null
        ip addr add "$IP" dev usb0 2>/dev/null
        ip link set usb0 up
    else
        ifconfig usb0 "$ADDR" netmask "$MASK" up
    fi
    log "usb0 = $IP"
}

start_sshd() {
    [ "$SSHD" = "1" ] || return 0
    if pgrep dropbear >/dev/null 2>&1 || pgrep sshd >/dev/null 2>&1; then
        log "ssh 服务已在运行"; return 0
    fi
    if command -v dropbear >/dev/null 2>&1; then
        mkdir -p /etc/dropbear
        dropbear -R 2>/dev/null || dropbear 2>/dev/null
        log "dropbear 已拉起"
    elif command -v /usr/sbin/sshd >/dev/null 2>&1 || command -v sshd >/dev/null 2>&1; then
        (sshd 2>/dev/null || /usr/sbin/sshd 2>/dev/null) && log "sshd 已拉起"
    else
        log "板上没有 dropbear/sshd —— ssh 登不进来，只能继续用串口"
    fi
}

start() {
    if ! ensure_configfs; then
        fallback_gether && start_sshd
        return $?
    fi
    [ -d "$G" ] && stop_quiet

    mkdir -p "$G" && cd "$G" || return 1
    echo 0x1d6b > idVendor      # Linux Foundation
    echo 0x0104 > idProduct     # Multifunction Composite
    echo 0x0100 > bcdDevice
    echo 0x0200 > bcdUSB
    mkdir -p strings/0x409
    (cat /etc/machine-id 2>/dev/null || echo ferry0001) > strings/0x409/serialnumber
    echo "ferry" > strings/0x409/manufacturer
    echo "ferry usb-net gadget" > strings/0x409/product
    mkdir -p configs/c.1/strings/0x409
    echo "net" > configs/c.1/strings/0x409/configuration
    echo 250 > configs/c.1/MaxPower

    HOST_MAC="$(mac_from_id 01)"
    DEV_MAC="$(mac_from_id 02)"

    case "$MODE" in
        rndis)
            mkdir -p functions/rndis.usb0
            echo "$HOST_MAC" > functions/rndis.usb0/host_addr 2>/dev/null
            echo "$DEV_MAC"  > functions/rndis.usb0/dev_addr  2>/dev/null
            # Windows 自动装驱动需要 os_desc
            echo 1        > os_desc/use 2>/dev/null
            echo 0xcd     > os_desc/b_vendor_code 2>/dev/null
            echo MSFT100  > os_desc/qw_sign 2>/dev/null
            mkdir -p functions/rndis.usb0/os_desc/interface.rndis 2>/dev/null
            echo RNDIS    > functions/rndis.usb0/os_desc/interface.rndis/compatible_id 2>/dev/null
            echo 5162001  > functions/rndis.usb0/os_desc/interface.rndis/sub_compatible_id 2>/dev/null
            ln -s configs/c.1 os_desc 2>/dev/null
            ln -s functions/rndis.usb0 configs/c.1/
            ;;
        ecm)
            mkdir -p functions/ecm.usb0
            echo "$HOST_MAC" > functions/ecm.usb0/host_addr 2>/dev/null
            echo "$DEV_MAC"  > functions/ecm.usb0/dev_addr  2>/dev/null
            ln -s functions/ecm.usb0 configs/c.1/
            ;;
        *)
            mkdir -p functions/ncm.usb0
            echo "$HOST_MAC" > functions/ncm.usb0/host_addr 2>/dev/null
            echo "$DEV_MAC"  > functions/ncm.usb0/dev_addr  2>/dev/null
            ln -s functions/ncm.usb0 configs/c.1/
            ;;
    esac

    if [ "$WITH_ACM" = "1" ]; then
        mkdir -p functions/acm.GS0 && ln -s functions/acm.GS0 configs/c.1/ 2>/dev/null \
            && log "已附带 USB 串口 (板端 /dev/ttyGS0)。想要登录 shell 可在 inittab 加: ttyGS0::respawn:/sbin/getty -L ttyGS0 115200 vt100"
    fi

    UDC="$(ls /sys/class/udc 2>/dev/null | head -1)"
    [ -n "$UDC" ] || { log "没有 UDC（这块板/这个口不支持 device 模式）"; return 1; }
    echo "$UDC" > UDC || { log "绑定 UDC 失败"; return 1; }
    sleep 1
    config_ip
    start_sshd
    log "完成。主机侧运行: fy usb net"
}

stop_quiet() {
    cd "$G" 2>/dev/null || return 0
    echo "" > UDC 2>/dev/null
    rm -f configs/c.1/ncm.usb0 configs/c.1/ecm.usb0 configs/c.1/rndis.usb0 configs/c.1/acm.GS0 os_desc/c.1 2>/dev/null
    rmdir functions/*/os_desc/interface.rndis 2>/dev/null
    rmdir functions/* configs/c.1/strings/0x409 configs/c.1 strings/0x409 2>/dev/null
    cd / && rmdir "$G" 2>/dev/null
}

status() {
    if [ -d "$G" ] && [ -s "$G/UDC" ]; then
        echo "gadget: up ($(cat "$G/UDC"))"
    else
        echo "gadget: down"
    fi
    (ip addr show usb0 2>/dev/null || ifconfig usb0 2>/dev/null) | sed 's/^/  /'
}

case "$1" in
    stop)   stop_quiet; log "已停止" ;;
    status) status ;;
    *)      start ;;
esac
