#!/usr/bin/env bash
# benches/runner/hardware-signature.sh
#
# Emits a single-line, self-authenticating hardware signature for the
# current host. Every bench run prefaces its output with this line so
# results carry their own provenance.
#
# Format (canonical form v1):
#
#   BENCH_HW v1: <field=value ...> sha=<64-hex>
#
# Fields are sorted lexically by key and shell-escaped (spaces -> \ ).
# The sha is computed over the canonical-form bytes with NO "sha="
# segment included (see canonicalize() below). This makes the line
# tamper-evident against accidental corruption; it is NOT a security
# property — a malicious actor would just recompute the hash. See
# benches/README.md for the threat model.
#
# Environment:
#   BENCH_TIER  required for real benches; 1 or 2. Unset → tier=unknown
#               (warning). Value other than 1|2 → hard error.
#
# Exit codes:
#   0  signature emitted on stdout
#   1  unsupported platform or invalid BENCH_TIER
#
# Runnable directly for diagnostics (`./hardware-signature.sh`) or
# sourced for testing (`. hardware-signature.sh; emit_signature`).

set -euo pipefail

_here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./hwsig-lib.sh
. "$_here/hwsig-lib.sh"

# ---------------------------------------------------------------------
# Field collectors. Each function prints one value on stdout, nothing
# else. Errors go to stderr; the field becomes "unknown" or "0"
# rather than aborting — the signature should describe what is
# knowable, not die on best-effort fields.
# ---------------------------------------------------------------------

_field_os()       { uname_os_normalize; }
_field_arch()     { uname_arch_normalize; }
_field_kernel()   { uname -r; }

_field_cores() {
    case "$(uname -s)" in
        Linux)  nproc 2>/dev/null || echo 0 ;;
        Darwin) sysctl -n hw.physicalcpu 2>/dev/null || echo 0 ;;
        *)      echo 0 ;;
    esac
}

_field_cpu() {
    local raw
    case "$(uname -s)" in
        Linux)
            raw=$(awk -F': ' '/^model name/ { print $2; exit }' /proc/cpuinfo 2>/dev/null || true)
            ;;
        Darwin)
            raw=$(sysctl -n machdep.cpu.brand_string 2>/dev/null || true)
            ;;
    esac
    [ -z "$raw" ] && raw=unknown
    trim_ws "$raw"
}

_field_cpu_mhz_max() {
    # Never emit an empty value — the canonical form requires every
    # field to have a non-empty value so space-separated parsing
    # stays unambiguous. Fall through lscpu -> /proc/cpuinfo -> 0.
    # Azure VMs and many containers have lscpu but no "CPU max MHz".
    local v=""
    case "$(uname -s)" in
        Linux)
            if command -v lscpu >/dev/null 2>&1; then
                v=$(lscpu 2>/dev/null | awk -F': +' '/CPU max MHz/ { printf "%d", $2; exit }')
            fi
            if [ -z "$v" ] && [ -r /proc/cpuinfo ]; then
                v=$(awk -F': ' '/cpu MHz/ { printf "%d", $2; exit }' /proc/cpuinfo 2>/dev/null)
            fi
            ;;
        Darwin)
            local hz
            hz=$(sysctl -n hw.cpufrequency_max 2>/dev/null || echo 0)
            v=$(( hz / 1000000 ))
            ;;
    esac
    [ -z "$v" ] && v=0
    printf '%s' "$v"
}

_field_ram_gb() {
    case "$(uname -s)" in
        Linux)
            awk '/^MemTotal:/ { printf "%d", int($2 / 1024 / 1024) }' /proc/meminfo 2>/dev/null || echo 0
            ;;
        Darwin)
            local bytes
            bytes=$(sysctl -n hw.memsize 2>/dev/null || echo 0)
            echo $(( bytes / 1024 / 1024 / 1024 ))
            ;;
        *) echo 0 ;;
    esac
}

_field_storage() {
    case "$(uname -s)" in
        Linux)
            # Primary block device = the one backing `/`. lsblk -no PKNAME
            # gives the parent kernel name; fall back to sdX/nvmeXnY.
            local root_dev pkname model
            root_dev=$(findmnt -no SOURCE / 2>/dev/null || echo "")
            [ -z "$root_dev" ] && { echo unknown; return; }
            pkname=$(lsblk -no PKNAME "$root_dev" 2>/dev/null | head -n1)
            [ -z "$pkname" ] && pkname=$(basename "$root_dev")
            model=$(lsblk -dno MODEL "/dev/$pkname" 2>/dev/null | head -n1 || echo "")
            trim_ws "${model:-unknown}"
            ;;
        Darwin)
            # diskutil info / doesn't exist; get root device first.
            local root_dev model
            root_dev=$(df / 2>/dev/null | awk 'NR==2 {print $1}')
            if [ -n "$root_dev" ]; then
                model=$(diskutil info "$root_dev" 2>/dev/null \
                    | awk -F': +' '/Device \/ Media Name|Media Name/ { print $2; exit }' \
                    || echo "")
                trim_ws "${model:-unknown}"
            else
                echo unknown
            fi
            ;;
        *) echo unknown ;;
    esac
}

_field_scheduler() {
    case "$(uname -s)" in
        Linux)
            local root_dev pkname sched_file
            root_dev=$(findmnt -no SOURCE / 2>/dev/null || echo "")
            [ -z "$root_dev" ] && { echo unknown; return; }
            pkname=$(lsblk -no PKNAME "$root_dev" 2>/dev/null | head -n1)
            [ -z "$pkname" ] && pkname=$(basename "$root_dev")
            sched_file="/sys/block/$pkname/queue/scheduler"
            if [ -r "$sched_file" ]; then
                # File contains `[active] alt1 alt2` — extract the bracketed one.
                sed -n 's/.*\[\([^]]*\)\].*/\1/p' "$sched_file" | head -n1 || echo unknown
            else
                echo unknown
            fi
            ;;
        *) echo unknown ;;
    esac
}

_field_tsc() {
    case "$(uname -s)" in
        Linux)
            if grep -qw constant_tsc /proc/cpuinfo 2>/dev/null \
               && grep -qw nonstop_tsc /proc/cpuinfo 2>/dev/null; then
                echo invariant
            else
                echo variable
            fi
            ;;
        Darwin)
            # Apple Silicon and recent Intel Macs all have invariant TSC.
            echo invariant
            ;;
        *) echo unknown ;;
    esac
}

_field_turbo() {
    case "$(uname -s)" in
        Linux)
            local f=/sys/devices/system/cpu/intel_pstate/no_turbo
            if [ -r "$f" ]; then
                case "$(cat "$f")" in
                    1) echo disabled ;;
                    0) echo enabled  ;;
                    *) echo unknown  ;;
                esac
            else
                echo unknown
            fi
            ;;
        *) echo unknown ;;
    esac
}

_field_mem_channels() {
    # Best-effort, root-only via dmidecode. Returns 0 when unknown.
    case "$(uname -s)" in
        Linux)
            if command -v dmidecode >/dev/null 2>&1 && [ "$(id -u)" = "0" ]; then
                # Count populated Memory Device entries with a non-null size.
                dmidecode -t memory 2>/dev/null \
                    | awk '/^Memory Device$/ { in_dev=1; next }
                           in_dev && /^Size: / && $2 != "No" && $2 != "Unknown" { count++; in_dev=0 }
                           in_dev && /^$/ { in_dev=0 }
                           END { print count+0 }'
            else
                echo 0
            fi
            ;;
        *) echo 0 ;;
    esac
}

_field_virt() {
    case "$(uname -s)" in
        Linux)
            if command -v systemd-detect-virt >/dev/null 2>&1; then
                local v
                v=$(systemd-detect-virt 2>/dev/null || echo none)
                [ "$v" = "none" ] && echo bare-metal || echo "$v"
            else
                echo unknown
            fi
            ;;
        Darwin)
            local v
            v=$(sysctl -n kern.hv_vmm_present 2>/dev/null || echo "")
            case "$v" in
                1) echo vmm ;;
                0) echo bare-metal ;;
                *) echo bare-metal ;;  # pre-Big-Sur: field absent -> assume bare-metal
            esac
            ;;
        *) echo unknown ;;
    esac
}

# ---------------------------------------------------------------------
# Tier validation (R4 rule)
# ---------------------------------------------------------------------

# validate_tier_argv "$@"
#   Validates BENCH_TIER against the caller's argv. No stdout output —
#   intended to be called at a script's top level so that `exit`
#   propagates to the user's shell. Call this BEFORE any `$(...)`
#   subshell that would otherwise swallow the exit.
#
#   Errors:
#     BENCH_TIER unset + argv has `bench` token  → exit 2
#     BENCH_TIER set but not in {1,2}            → exit 1
validate_tier_argv() {
    local tier="${BENCH_TIER:-}"
    case "$tier" in
        1|2) return 0 ;;
        "")
            local looks_like_bench=0
            for a in "$@"; do
                case "$a" in
                    bench|*--bench*|*cargo*bench*) looks_like_bench=1 ;;
                esac
            done
            if [ "$looks_like_bench" = 1 ]; then
                echo "error: BENCH_TIER must be set to 1 or 2 when running benches" >&2
                exit 2
            fi
            # Soft-warn path: warning emitted by resolve_tier_value on
            # the actual signature-emission call. Don't double-warn here.
            return 0
            ;;
        *)
            echo "error: BENCH_TIER='$tier' is invalid; must be 1 or 2" >&2
            exit 1
            ;;
    esac
}

# resolve_tier_value
#   Returns the tier value on stdout. Called from inside the
#   signature-emission subshell, so this function does NOT exit on
#   error — validate_tier_argv is expected to have run first at the
#   top level. On bad input it still prints a warning + returns
#   "unknown" so the signature remains emitable.
resolve_tier_value() {
    local tier="${BENCH_TIER:-}"
    case "$tier" in
        1|2) printf '%s' "$tier" ;;
        "")
            echo "warning: BENCH_TIER unset; signature will report tier=unknown" >&2
            printf 'unknown'
            ;;
        *)
            # Shouldn't happen if validate_tier_argv ran, but be
            # defensive — emit unknown rather than hang or crash.
            echo "warning: BENCH_TIER='$tier' invalid; signature will report tier=unknown" >&2
            printf 'unknown'
            ;;
    esac
}

# resolve_tier "$@"
#   Compatibility shim for direct invocations of hardware-signature.sh
#   (no wrapper). Validates THEN resolves. Exits on hard-fail cases.
resolve_tier() {
    validate_tier_argv "$@"
    resolve_tier_value
}

# ---------------------------------------------------------------------
# Canonicalization + signature assembly
# ---------------------------------------------------------------------

# canonicalize_fields
#   Reads key=value pairs on stdin (one per line), trims whitespace
#   from values, shell-escapes them, sorts by key, joins with single
#   spaces, and prints the result with no trailing newline.
canonicalize_fields() {
    awk -F= '
        NF >= 2 {
            key = $1
            # Reassemble value (in case value contained =).
            val = $2
            for (i = 3; i <= NF; i++) val = val "=" $i
            print key "\t" val
        }
    ' | LC_ALL=C sort -k1,1 -t$'\t' \
      | while IFS=$'\t' read -r key val; do
            val=$(trim_ws "$val")
            val=$(value_encode "$val")
            printf '%s=%s ' "$key" "$val"
        done \
      | sed 's/ $//'
}

# emit_signature [argv ...]
#   Collects all fields, runs canonicalize_fields, computes sha,
#   prints the full `BENCH_HW v1: ... sha=...` line to stdout.
emit_signature() {
    local tier
    tier=$(resolve_tier_value)

    local fields
    fields=$(cat <<EOF
os=$(_field_os)
arch=$(_field_arch)
kernel=$(_field_kernel)
cores=$(_field_cores)
cpu=$(_field_cpu)
cpu_mhz_max=$(_field_cpu_mhz_max)
ram_gb=$(_field_ram_gb)
storage=$(_field_storage)
scheduler=$(_field_scheduler)
tsc=$(_field_tsc)
turbo=$(_field_turbo)
mem_channels=$(_field_mem_channels)
virt=$(_field_virt)
tier=$tier
EOF
)
    local canonical sha
    canonical=$(printf '%s\n' "$fields" | canonicalize_fields)
    sha=$(sha256_of_string "$canonical")
    printf 'BENCH_HW v1: %s sha=%s\n' "$canonical" "$sha"
}

# When executed directly (not sourced), validate at top level (so
# exit propagates) and then emit the signature.
if [ "${BASH_SOURCE[0]}" = "${0}" ]; then
    validate_tier_argv "$@"
    emit_signature
fi
