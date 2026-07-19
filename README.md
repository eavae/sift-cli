# sift

> **CN A-share / HK stock data CLI — built to be piped into AI agents**

[中文版 README](./README_cn.md)

`sift` is a single-binary Rust CLI that pulls CN A-share + HK + US stock data — listings, financial statements, announcements, quote snapshots, OHLC bars, and PDF/OCR text extracts — from public endpoints (cninfo / 东方财富 / sina / tencent / 巨潮) and emits Unix-friendly TSV / NDJSON / aligned tables.

## What & Why

`sift` exists for one reason: **let LLM agents (Claude, Kimi, Hermes, OpenClaw, …) read Chinese-market financials and OHLC bars as fluently as a human analyst.**

Most investment-data tools target humans (GUIs) or assume an SDK consumer (deeply-nested JSON, stateful sessions). Neither works well for tool-use loops: GUIs can't be scripted, and nested JSON wastes tokens and breaks LLM parsing. `sift` goes the other way — **a pure-stdout CLI** where every command emits `#header\tcol\tcol\n` TSV (awk/pandas/Polars friendly) or NDJSON (one object per line). Any tool-using model (Claude Code, OpenCode, Kimi K2, …) can pipe results straight back to the LLM at zero parser cost.

In one line: **`sift` is plumbing — turn the LLM tap and stock data comes out.**

## Highlights

| Feature | What it gives you |
|---|---|
| **🤖 Agent-first output** | Default TSV with `#header` row (doubles as column names and a comment marker, VCF/MAF convention). `--format json` is NDJSON. Any model parses it reliably. |
| **🔗 Pipe-friendly** | `sift search 银行 --format json \| jq -r .code \| xargs sift quote` — subcommands compose naturally. |
| **📊 Multi-source financials, first-success-wins** | Eastmoney + Sina (+ future cninfo) race in parallel; whoever returns first wins. Pin with `--source` for reproducible runs. |
| **🗂 Full announcement pipeline** | `announce list` → browse, `show` → metadata, `download` → PDF, `extract` → Markdown with OCR escalation for scanned pages. |
| **📈 Quotes + OHLC bars** | `quote` for live snapshots, `bars` for daily/weekly/monthly with pre/none/post adjustment. |
| **🏷 Verbatim item labels** | Financial line items keep their raw upstream names — no lossy remapping. A-share (东方财富) uses EM's English column codes (`TOTAL_OPERATE_INCOME`), HK/sina use their native Chinese labels. `--items` filters against those exact labels. |
| **💾 Local cache** | Listings (24h file cache), financials (DuckDB + 3-tier TTL, fresher periods get shorter TTL), announcement metadata (DuckDB, no TTL), PDFs (forever). Re-runs return in ms. |
| **🔒 Offline-first, graceful degradation** | `$HOME` unresolvable → caches disable but commands keep working. Per-symbol failure → `[warn]` on stderr, other symbols still print to stdout. Never crashes the whole run. |
| **🚀 Single static binary** | DuckDB bundled. No Python / Node / system libs. One `~/.local/bin/sift`, done. |

## Agent usage patterns

Add `sift` to your agent's allowed-tool list (Claude Code, OpenCode, Kimi K2, etc.) and it can do this:

```text
User: Show me Moutai's net-profit trend over the last 4 quarters vs. peers.

Agent calls:
  1. sift search 茅台 --limit 1 --format json           → resolves 600519
  2. sift report income 600519 --last 4 --unit yi --format tsv
  3. sift search 白酒 --limit 5 --format json | ...     → peer codes
  4. sift report income <codes> --last 4 --unit yi --format tsv
  5. (LLM synthesizes the two TSVs into prose + chart)
```

```text
User: Summarize Moutai's 2024 annual report, pages 1-30.

Agent calls:
  1. sift announce list 600519 --type 年报 --start 2024-01-01 --format json
  2. sift announce download <id> -o /tmp
  3. sift extract <id> --mode auto --pages 1-30 > /tmp/report.md   # scanned pages auto-OCR
  4. (LLM reads the markdown and writes the summary)
```

```text
User: Which A-shares are rallying on heavy volume today?

Agent calls:
  1. sift search 600 --limit 50 --format json | jq -r .code | xargs sift quote --format tsv
  2. (LLM ranks by pct_change and compares against 5-day avg volume)
```

## Install

### Prebuilt binary (recommended)

One-liner — auto-detects OS + arch, downloads from GitHub Releases, and installs a single binary into the first writable directory on your `PATH` (falling back to `~/.local/bin`):

```bash
curl -fsSL https://raw.githubusercontent.com/eavae/sift-cli/main/scripts/install.sh | bash
```

Optional env overrides:

```bash
SIFT_VERSION=v0.2.0 \
SIFT_INSTALL_DIR=/usr/local/bin \
  curl -fsSL https://raw.githubusercontent.com/eavae/sift-cli/main/scripts/install.sh | bash
```

Supported targets: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc`. The script verifies the SHA-256 checksum that ships next to each archive.

**Where `github.com` is blocked (e.g. mainland China):** the script auto-detects an unreachable GitHub and pulls the binary through a public mirror. Since fetching the script itself also goes through GitHub, route that through the mirror too:

```bash
curl -fsSL https://cdn.gh-proxy.org/https://raw.githubusercontent.com/eavae/sift-cli/main/scripts/install.sh | bash
```

Force the behavior with `SIFT_MIRROR`: `auto` (default), `off` (direct only), or a mirror URL (e.g. `SIFT_MIRROR=https://cdn.gh-proxy.org`).

**Windows** (`x86_64-pc-windows-msvc`) ships as a `.zip`. Run the same one-liner **under [Git Bash](https://git-scm.com/downloads) / MSYS2** and it detects Windows, downloads the `.zip`, and installs `sift.exe` for you. (Prefer to do it by hand? Grab the archive from the [Releases](https://github.com/eavae/sift-cli/releases) page, extract `sift.exe`, and put it on your `PATH`.) The store lives at `%USERPROFILE%\.sift` (e.g. `C:\Users\you\.sift`); set `HOME` to override the whole tree.

### Build from source (fallback)

If your platform isn't covered by the releases, or you want bleeding-edge `main`:

```bash
git clone https://github.com/eavae/sift-cli.git
cd sift-cli
./install.sh                                  # builds release + copies to ~/.local/bin
# or manually:
cargo build --release && cp target/release/sift ~/.local/bin/
```

> First build ~3 min (DuckDB statically linked), incremental seconds. Requires Rust ≥ 1.95.

On **Windows**, use the MSVC toolchain (`rustup default stable-x86_64-pc-windows-msvc`) plus the [Visual Studio C++ Build Tools](https://visualstudio.microsoft.com/visual-studio-build-tools/) — the bundled DuckDB is C++ and needs a C++ compiler. Then `cargo build --release` produces `target\release\sift.exe`. (`install.sh` is bash-only; copy the `.exe` onto your `PATH` yourself.)

### Verify

Make sure `~/.local/bin` is on `PATH`, then:

```bash
sift --help
sift search 茅台
sift report income 600519 --last 4 --unit yi
```

## Command cheat sheet

```bash
sift search <kw> [--limit N] [--no-cache]                  # fuzzy search (code / name / pinyin initials)
sift report {income|balance|cashflow|indicator|periods} <code...> [--last N | --period 2024Q3 | --start --end]
sift announce {list|show|download|types} ...               # announcement browsing / downloading
sift extract <id|path> [--pages 1-30] [--mode fast|fine|auto]   # PDF → Markdown
sift quote <code...>                                        # live snapshot
sift bars  <code...> [--period daily|weekly|monthly] [--limit N] [--adjust pre|none|post]
```

Every command accepts `--format tsv|json` (omit for human-aligned table). Multi-symbol calls are best-effort: one failure doesn't sink the rest.

Symbol forms: bare 6 digits = A-share (`600519`), bare 5 digits = HK (`00700`), `600519.SH` / `00700.HK` suffixes and `sh600519` prefixes all work. **Indices** need an explicit exchange prefix and are served by `quote` / `bars` only: `sh000001` = 上证指数, `sz399001` = 深证成指 (output keeps the lowercase prefixed form; `report` / `announce` reject indices). Note `000001` without a prefix is always 平安银行 the stock, never the index.

## Environment variables

### 1. PaddleOCR (required for `sift extract --mode fine|auto`)

`extract` runs purely locally in `fast` mode (zero API calls). When you hit scanned pages and need real OCR, switch to `fine` or `auto` — at which point `sift` calls a PaddleOCR cloud backend. **Two configuration modes are supported; pick the one that matches your account.**

#### ① Token mode (personal / self-hosted)

For **PaddleX official hosted service** or your own self-hosted PaddleOCR deployment. One-shot token auth — simplest path.

```bash
export PADDLEOCR_API_BASE="https://<your-paddleocr-endpoint>"   # no trailing /layout-parsing
export PADDLEOCR_API_TOKEN="<your-token>"
```

- Single synchronous HTTP round-trip, lowest latency.
- Good for: individual developers, self-hosted PaddleX, free-tier trials.

#### ② OAuth mode (enterprise / Baidu AI Cloud)

For **Baidu AI Open Platform** enterprise accounts. API Key/Secret auth via OAuth, async task endpoint.

```bash
export PADDLEOCR_API_KEY="<your-baidu-api-key>"
export PADDLEOCR_SECRET_KEY="<your-baidu-secret-key>"
# optional override (rarely needed):
export SIFT_BAIDU_HOST="https://aip.baidubce.com"
```

- Auto OAuth → access token (30-day TTL, in-process cache).
- Async pipeline: submit task → poll → download structured result (layout / tables / images).
- Good for: enterprise volume, per-account billing, Baidu-cloud compliance.

#### Detection order

At OCR startup `sift` checks **Token → OAuth**:

1. Both `PADDLEOCR_API_BASE` + `PADDLEOCR_API_TOKEN` non-empty → Token mode wins.
2. Otherwise both `PADDLEOCR_API_KEY` + `PADDLEOCR_SECRET_KEY` non-empty → OAuth mode.
3. Neither configured → `--mode fine|auto` errors with a hint.

Configure only one. If both are set, Token mode takes precedence.

### 2. Other optional env vars

| Variable | Purpose | Default |
|---|---|---|
| `SIFT_DOWNLOAD_DELAY_MS` | Sleep between PDFs during `announce download` (anti-scrape), in ms | `0` |
| `SIFT_CNINFO_BASE` | cninfo API base (testing / mirror) | `http://www.cninfo.com.cn` |
| `SIFT_EM_HSF10_BASE` | Eastmoney F10 base | upstream default |
| `SIFT_EM_DATACENTER_BASE` | Eastmoney datacenter-web base | upstream default |
| `SIFT_EM_QUOTE_BASE` | Eastmoney quote base | upstream default |
| `SIFT_EM_BARS_BASE` | Eastmoney bars base | upstream default |
| `SIFT_TENCENT_BARS_BASE` | Tencent bars base | upstream default |
| `SIFT_SINA_BASE` | Sina financials base | upstream default |

The last seven exist for mockito tests and internal mirrors — production usually leaves them unset.

## Cache layout

```
~/.sift/cache/
├── cninfo/{szse,hke}_stock.json     # F1 listings, 24h TTL
├── announcements/<id>.pdf           # F3 announcement PDFs, no TTL
├── announcements/<id>/images/       # F4 extracted images
└── records.duckdb                   # F2 financials + F3 announce metadata
```

Force-refresh by deleting the corresponding file (or passing `--no-cache` where supported).

## Status

- ✅ **F1 search** — fuzzy listing lookup (4/4)
- ✅ **F2 report** — financials (5/5) + transposed layout + `--source` pin + verbatim item labels
- ✅ **F3 announce** — list / show / download / types
- ✅ **F4 extract** — fast / fine / auto modes
- ✅ **F5 realtime** — quote / bars
- 🚧 **US market** — enum present but `Symbol::parse` rejects US; not wired end-to-end

See feature folders under [`docs/`](./docs/) for `README.md` (contract) + `story-NN-*.md` (execution cards).

## License

(Add a LICENSE file — MIT / Apache-2.0 / proprietary as appropriate.)
