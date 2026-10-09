<p align="left">
  <img src="docs/images/logo.png" width="32" vertical-align="middle" />
  <b>Making fragmented information flow effortlessly.</b>
</p>

---

<div align="center">
  <img src="docs/images/logo.png" alt="TieZ Hero Logo" width="300" />

  ### **STAY FAST. STAY SYNCED.**

  | STARS | VERSION | LICENSE | PLATFORM |
  | :--- | :--- | :--- | :--- |
  | [![Stars](https://img.shields.io/github/stars/jimuzhe/tiez-clipboard?label=STARS&style=for-the-badge&color=4CAF50)](https://github.com/jimuzhe/tiez-clipboard/stargazers) | [![Version](https://img.shields.io/github/v/release/jimuzhe/tiez-clipboard?label=VERSION&style=for-the-badge&color=2196F3)](https://github.com/jimuzhe/tiez-clipboard/releases) | [![License](https://img.shields.io/badge/LICENSE-GPL--3.0-FF9800?style=for-the-badge)](https://www.gnu.org/licenses/gpl-3.0) | [![Platform](https://img.shields.io/badge/PLATFORM-WIN%20%2F%20MAC-f44336?style=for-the-badge)](https://github.com/jimuzhe/tiez-clipboard/releases) |

  [English](./README.md) | [简体中文](./README.zh-CN.md)
</div>

---

<div align="center">

## Theme Gallery

Explore 4 elegant themes designed for every workspace and efficiency scenarios.

  <table>
    <tr>
      <td align="center"><b>Frosted Glass</b><br><img src="docs/images/毛玻璃.png" width="220" /></td>
      <td align="center"><b>Notebook Style</b><br><img src="docs/images/书.png" width="220" /></td>
      <td align="center"><b>Sticky Note</b><br><img src="docs/images/便利贴.png" width="220" /></td>
      <td align="center"><b>3D Interaction</b><br><img src="docs/images/3d.png" width="220" /></td>
    </tr>
  </table>
</div>

---

## Why TieZ?

| Performance | Practicality | Privacy | Sync |
| :--- | :--- | :--- | :--- |
| **Instant Access**<br>Native listeners and Rust core ensure absolute speed. | **Power Workflows**<br>Rich text, tags, and AI-assisted actions. | **Local & Private**<br>Local-first storage with smart masking for sensitive data in previews. | **Cloud Fluent**<br>Seamless WebDAV and MQTT cross-device sync. |

---

## Key Features

### Core Experience
- **Native Efficiency**: Built with Tauri 2 and Rust for minimum memory footprint.
- **Smart Capture**: Automatically collects text, rich text (HTML), images, and file paths.
- **Modern UI**: Supports Mica/Acrylic effects and Dark/Light modes with **4 elegant theme styles**.
- **Edge Docking**: Automatically hides at the screen edge to stay out of your way.

### Management & Enhancements
- **Tag System**: Organize your history with custom multi-color tags.
- **Emoji Library**: Comprehensive built-in emoji management for quick access.
- **Advanced Settings**: Granular control over cleanup rules and app behavior.
- **Privacy Masking**: Auto-masks sensitive info like IDs and phone numbers in previews.

### Networking & Transport
- **WebDAV Sync**: Your data, your cloud. Complete cross-device history.
- **LAN File Transfer**: Seamlessly move items between devices on the same network.
- **Verifcation Code Sync**: Instant transfer of OTP codes to your active device.
- **MQTT Connectivity**: Optimized for real-time synchronization between devices.

### Productivity Tools
- **External Collaboration**: Open items in external editors with auto-sync back.
- **Global Search**: Find anything by content, source app, or date.
- **Sequential Paste**: Optimized workflow for high-frequency copy-paste tasks.

---

## Local Patches

This fork adds search and startup fixes on top of upstream `v0.3.3`.
Attribution: [jimuzhe/tiez-clipboard](https://github.com/jimuzhe/tiez-clipboard) — GPL-3.0.

### Search: FTS5 trigram index

Upstream searched with `content LIKE '%' || ? || '%'`. SQLite cannot use an index for
that shape, so every keystroke scanned the whole table — and the rarer the term, the
slower it got (a no-match term took ~1.1 s, a common one ~2.1 s).

Replaced with an FTS5 external-content table using the `trigram` tokenizer, which is
the only officially supported way to keep substring semantics *and* use an index:

```sql
CREATE VIRTUAL TABLE clipboard_fts USING fts5(
    content, source_app,
    content='clipboard_history', content_rowid='id',
    tokenize='trigram'
);
```

Details that matter:

- **Two-phase query.** Fetch candidate `id`s with FTS, then load full rows for just
  those. Putting wide columns in `ORDER BY` makes SQLite spill to a temp B-tree and
  is measurably *slower* than the original.
- **Candidate sets are re-checked with `LIKE`.** The index is an accelerator; `LIKE`
  remains the source of truth for correctness.
- **Terms shorter than 3 characters fall back to `LIKE`.** trigram cannot match them,
  and SQLite's docs warn it degrades to a full scan.
- **User input never enters MATCH syntax raw.** `AND`/`OR`/`*`/`(` are FTS operators;
  terms are wrapped as escaped quoted phrases.
- **Background backfill.** Building the index on a 473 MB database took ~62 s, so
  migration only does DDL and a background thread backfills in batches, releasing the
  connection between batches so clipboard capture is never blocked. Until it finishes,
  searches transparently use the old path.

### Startup: silent-crash fix

`tauri-plugin-http` was registered but never used — no frontend code imports it and no
capability grants `http:*`. Its `setup` hook nonetheless hard-fails the whole app when
it cannot create its cookie store (`%LOCALAPPDATA%\<id>\.cookies`). On such machines
TieZ exited **before writing a single log line**. The registration is removed.

A crash reporter and a 20-second startup watchdog were added so the next failure says
something specific instead of leaving an invisible process.

### Build correctly

`cargo build --release` produces a **hollow exe** with no UI — it silently skips asset
embedding because only the Tauri CLI passes `DEP_TAURI_DEV`, and then tries to load
`http://localhost:1420`. Always build with the Tauri CLI:

```bash
npm install
npx tauri build --no-bundle
```

Verify before deploying — a bare `cargo build` will not report this:

```powershell
$a = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes('tiez-app.exe'))
$a.Contains('assets/index-')     # must be True
```

---

## Installation

### Platform Support
| Platform | Requirement | Output |
| :--- | :--- | :--- |
| **Windows** | Windows 10/11 (x86/x64)<br>*(Windows 11 Recommended)* | `.exe` / **`.zip` (Portable)** |
| **macOS** | Sierra 10.15+ <br>(Apple Silicon / Intel) | `.dmg` |
| **Linux** | Support Coming Soon | TBD |

[**Download the Latest Release →**](https://github.com/jimuzhe/tiez-clipboard/releases)

---

## Star History

<div align="center">
  <a href="https://star-history.com/#jimuzhe/tiez-clipboard&Date">
    <img src="https://api.star-history.com/svg?repos=jimuzhe/tiez-clipboard&type=Date" alt="Star History Chart" width="800" />
  </a>
</div>

---

## Community & Support

If TieZ makes your life easier, consider supporting the journey.

<div align="center">
  <table style="border: none;">
    <tr>
      <td align="center" style="border: none;">
        <p><strong>WeChat</strong></p>
        <img src="docs/images/wx.jpeg" alt="WeChat" width="180" height="180" />
      </td>
      <td align="center" style="border: none;">
        <p><strong>Alipay</strong></p>
        <img src="docs/images/zfb.jpeg" alt="Alipay" width="180" height="180" />
      </td>
      <td align="center" style="border: none;">
        <p><strong>QQ Group</strong></p>
        <img src="docs/images/qq.jpeg" alt="QQ Group" width="180" height="180" />
      </td>
    </tr>
  </table>
  <br>
  <p>Your support keeps the project active and the developer caffeinated!</p>
  <a href="https://tiez.name666.top/zh/sponsors.html"><strong>View Sponsor List</strong></a>
</div>

---

<div align="center">
  Built with technical precision for every efficient developer.
  <br>
  <b>Please consider leaving a Star if you find this project useful.</b>
</div>
