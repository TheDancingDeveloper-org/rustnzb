#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

cat <<'BANNER'
+=====================================================+
|  benchnzb: SABnzbd vs rustnzb                      |
+=====================================================+
BANNER

usage() {
    echo "Usage: $0 [OPTIONS]"
    echo ""
    echo "Options:"
    echo "  --scenarios GROUP   verify|verify-fault|quick|medium|speed|postproc|full"
    echo "  --no-cleanup        Keep containers after run"
    echo "  --help              Show this help"
    echo ""
    echo "Scenario groups:"
    echo "  quick    - 5 GB raw download only (~5 min)"
    echo "  medium   - 5+10 GB, all test types (~30 min)"
    echo "  speed    - 5+10+50 GB raw download (~60 min)"
    echo "  postproc - 5+10 GB par2+unpack (~45 min)"
    echo "  full     - All 9 scenarios (5/10/50 x raw/par2/unpack)"
    exit 0
}

SCENARIOS="${SCENARIOS:-quick}"
CLEANUP=1

while [[ $# -gt 0 ]]; do
    case "$1" in
        --scenarios)   SCENARIOS="$2"; shift 2 ;;
        --no-cleanup)  CLEANUP=0; shift ;;
        --help|-h)     usage ;;
        *) echo "Unknown: $1"; usage ;;
    esac
done

export SCENARIOS

# Stop any prior fixture before replacing bind-mounted state. Otherwise a
# previous rustnzb process can still write its in-memory default config after
# this script has seeded the next run's config.
docker compose down -v 2>/dev/null || true

mkdir -p results

# Clean state directories for fresh run (removes stale databases/history)
rm -rf state/sabnzbd state/rustnzb
mkdir -p state/sabnzbd state/rustnzb

# Seed config files
cp configs/sabnzbd.ini state/sabnzbd/sabnzbd.ini
cp configs/rustnzb.toml state/rustnzb/config.toml

LOGFILE="results/run_$(date +%Y%m%d_%H%M%S).log"

echo "[*] Scenarios:   $SCENARIOS"
echo "[*] Log file:    $LOGFILE"
echo "[*] Building (first run compiles Rust — takes a few minutes)..."
echo ""

docker compose up --build -d
set +e
docker compose wait orchestrator
EXIT_CODE=$?
set -e
docker compose logs --no-color | tee "$LOGFILE"

[[ "$CLEANUP" == "1" ]] && docker compose down -v 2>/dev/null || true

echo ""
echo "========================================"
echo "  Results: $(pwd)/results/"
echo "  Log:     $LOGFILE"
ls -lh results/ 2>/dev/null | tail -5
echo "========================================"
exit "${EXIT_CODE}"
