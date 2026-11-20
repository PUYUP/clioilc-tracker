# clioilc

CLI untuk memantau file log dari tool AI (Claude Code, Gemini CLI, dll.):

- **mencatat event perubahan file** (dibuat, diubah, di-rename, dihapus) ke log lokal;
- **membaca isi file secara realtime**, seperti `tail -F` untuk banyak file sekaligus. Hanya
  byte yang baru ditambahkan yang dibaca, jadi file berukuran ratusan MB tetap ringan;
- **mengumpulkan token usage** (model, token, sukses/error per respons dan per pemanggilan
  tool) ke SQLite;
- **mencatat apa yang dikerjakan agent setiap hari** (prompt, balasan, thinking, aksi tool)
  sebagai timeline yang hanya bisa dilihat pemiliknya.

Data dipisah menjadi metadata yang **aman dibagikan ke publik** dan isi percakapan yang
**privat**. Keduanya sudah disiapkan untuk sync ke dashboard web.

```sh
# token usage tiap balasan Claude, realtime
cargo run -q --release -- watch --follow \
  | jq --unbuffered -cR 'sub("^[^\t]*\t"; "") | fromjson
      | select(.type == "assistant") | .message.usage'
```

> Kenapa tidak pakai `cut -f2- | jq`? Lihat [Jebakan: buffering](#jebakan-buffering).

---

## Daftar isi

- [Build](#build)
- [Perintah](#perintah)
  - [`watch`](#watch): catat event perubahan
  - [`watch --follow`](#watch---follow): baca isi file realtime
  - [`history`](#history) dan [`summary`](#summary)
  - [`usage import` / `usage report`](#usage): token usage
  - [`activity`](#activity): apa yang dikerjakan agent hari ini
  - [`login` / `sync` / `logout`](#login--sync--logout): kirim data ke dashboard
  - [`service`](#service): jalankan watcher otomatis di background
  - [`add mod`](#add-mod): buat reader baru
- [Format output `--follow`](#format-output---follow)
- [Contoh pipeline](#contoh-pipeline)
- [Membaca token Claude](#membaca-token-claude)
- [Jebakan: buffering](#jebakan-buffering)
- [Database dan privasi](#database-dan-privasi)
- [File data](#file-data)
- [Perilaku dan batasan](#perilaku-dan-batasan)
- [Struktur workspace](#struktur-workspace)

---

## Build

Butuh Rust stable (`rustup default stable`).

```sh
cargo build --release
./target/release/clioilc --help
```

Semua contoh di bawah memakai `cargo run -q --release -- <perintah>`. `-q` menyembunyikan
pesan kompilasi dari cargo, dan `--` memisahkan argumen cargo dari argumen clioilc. Kalau
binary-nya sudah di-build, ganti dengan `./target/release/clioilc <perintah>`.

## Perintah

Opsi global:

| Opsi | Default | Keterangan |
|---|---|---|
| `--data-dir <DIR>` | `~/.clioilc` | Tempat menyimpan `events.jsonl` dan `offsets.json` |

### `watch`

Memantau direktori sebuah reader secara rekursif:

- mencatat event perubahan file ke `events.jsonl`;
- membaca setiap baris baru dan menyimpan token usage ke `usage.db` (lihat [`usage`](#usage)).

```sh
cargo run -q --release -- watch                    # reader pertama (claude): ~/.claude/projects
cargo run -q --release -- watch --reader gemini    # ~/.gemini
cargo run -q --release -- watch --path ~/lain      # direktori lain, tetap pakai aturan reader claude
```

| Opsi | Keterangan |
|---|---|
| `--reader <NAMA>` | `claude` (default) atau `gemini` |
| `--path <DIR>` | Ganti direktori yang dipantau |
| `--follow` | Juga cetak baris baru ke stdout (lihat di bawah) |

- Hanya file dengan ekstensi yang dilacak reader yang diproses (`.json` dan `.jsonl` untuk
  `claude` dan `gemini`).
- Event di-*debounce*: rentetan tulisan ke satu file dicatat sebagai **satu** event setelah
  file itu diam selama 1 detik. File yang terus ditulis tetap dicatat paling lambat setiap
  10 detik.
- Pesan status dan error ditulis ke **stderr**, jadi stdout tetap bersih untuk di-pipe.
- **Ctrl+C** pertama menghentikan watcher dengan rapi: event yang tertunda dicatat dan
  posisi baca disimpan. Ctrl+C kedua langsung keluar.

### `watch --follow`

`watch` selalu membaca baris baru (untuk mengisi `usage.db`). `--follow` hanya menambahkan
satu hal: setiap **baris baru yang sudah lengkap** juga dicetak ke stdout secara realtime
(latency sekitar 10–20 ms).

```sh
cargo run -q --release -- watch --follow
```

Dari mana pembacaan dimulai:

| Situasi | Mulai dari |
|---|---|
| Pertama kali dijalankan, file sudah ada | **akhir file**. Isi lama tidak dibaca ulang. |
| File baru yang muncul saat watcher berjalan | awal file |
| Dijalankan lagi setelah berhenti | posisi terakhir yang tersimpan di `offsets.json`, jadi baris yang ditulis saat watcher mati tetap terkirim |

Dengan kata lain, `watch` memproses apa yang **ditulis mulai sekarang**, bukan riwayat lama.
Riwayat lama diisi dengan [`usage import`](#usage).

Untuk mulai dari nol lagi (abaikan posisi tersimpan), hapus `~/.clioilc/offsets.json`.

### `history`

Menampilkan event yang tercatat.

```sh
cargo run -q --release -- history --limit 20
cargo run -q --release -- history --project -Volumes-SSD1-Private-clioilc-tracker
cargo run -q --release -- history --reader claude --path subagents
```

| Opsi | Keterangan |
|---|---|
| `--reader <NAMA>` | Hanya reader ini |
| `--project <SLUG>` | Hanya project ini (nama folder di bawah `~/.claude/projects`) |
| `--path <TEKS>` | Path mengandung teks ini |
| `--limit <N>` | Hanya N event terakhir |

### `summary`

Jumlah event per reader dan project. Menerima filter yang sama dengan `history` (tanpa
`--limit`).

```sh
cargo run -q --release -- summary
```

### `usage`

Token usage dari log Claude Code, disimpan di `usage.db` (SQLite). `watch` mengisinya secara
otomatis; `usage import` mengisi riwayat yang sudah ada.

```sh
# 1. isi riwayat (sekali, aman diulang: data tidak terhitung dobel)
cargo run -q --release -- usage import

# 2. pantau terus untuk data baru
cargo run -q --release -- watch

# lihat hasilnya
cargo run -q --release -- usage report               # per model (default)
cargo run -q --release -- usage report --by project
cargo run -q --release -- usage report --by day
```

Contoh output:

```
                   msgs  errors  input  cache_write   cache_read     output  tools  tool_err
claude-opus-5-5   2,170       0  4,684    6,185,614  750,354,650  3,025,408  2,309        44
claude-haiku-5-5     26       0     54      392,850      554,371     70,976     68         0
<synthetic>           4       4      0            0            0          0      0         0
TOTAL             2,200       4  4,738    6,578,464  750,909,021  3,096,384  2,377        44
not yet synced: 2200 message(s), 2378 tool call(s)
```

| Kolom | Arti |
|---|---|
| `msgs` | jumlah respons model, sudah di-dedupe per `message.id` |
| `errors` | respons yang gagal di sisi API (`rate_limit`, `billing_error`, …). Model-nya tercatat `<synthetic>` |
| `input` … `output` | jumlah token (lihat [field `usage`](#struktur-log-claude-code)) |
| `tools` / `tool_err` | pemanggilan tool dan berapa yang hasilnya error |

| Opsi `import` | Keterangan |
|---|---|
| `--reader <NAMA>` | reader yang dipakai (saat ini hanya `claude` yang punya parser) |
| `--path <DIR>` | impor dari direktori lain |

Catatan:

- `import` dan `watch` berjalan terpisah. Baris yang ditulis *di antara* selesainya `import` dan
  mulainya `watch` tidak tertangkap. Jalankan `usage import` lagi untuk menutup celah itu;
  dedupe membuatnya aman.
- `import` membaca 72 MB log dalam sekitar 0,3 detik: baris yang tidak membawa usage dilewati
  sebelum di-parse.
- Tool call berstatus `NULL` (pending) artinya hasilnya belum ditulis, misalnya tool-nya
  masih berjalan.

### `activity`

Timeline per hari tentang apa yang dikerjakan agent: prompt kamu, balasan agent, dan setiap
aksi tool beserta hasilnya. Datanya diambil dari tabel privat di `usage.db`, jadi isi dulu
dengan `usage import` dan/atau biarkan `watch` berjalan.

```sh
cargo run -q --release -- activity                         # hari ini (waktu lokal)
cargo run -q --release -- activity --date 2026-11-19
cargo run -q --release -- activity --project clioilc       # nama project mengandung teks ini
cargo run -q --release -- activity --thinking              # sertakan proses berpikir agent
cargo run -q --release -- activity --full                  # teks lengkap, bukan satu baris
```

Contoh output:

```
Activity on 2026-11-20

== -Volumes-SSD1-Private-clioilc-tracker  (session 5ca3976e)
07:51:08  you    analisa project ini refactor yang perlu
07:51:12  agent  Saya mulai dengan melihat struktur project.
07:51:13  tool   Bash [ok] List project files and Rust line counts: find . -path ./clioilc/target …
07:52:32  you    ya kerjakan semua
07:57:38  tool   Bash [ERROR] End-to-end test of watch, history, summary, errors: cargo build … -> Exit code 1
```

| Penanda | Arti |
|---|---|
| `you` | prompt yang kamu ketik (tanpa konteks yang disisipkan IDE/sistem) |
| `agent` | teks balasan agent |
| `think` | proses berpikir agent (hanya dengan `--thinking`) |
| `tool` | aksi tool: nama, `[ok]` / `[ERROR]` / `[...]` (belum selesai), ringkasan aksi, dan potongan pesan error |
| `[subagent]` | dikerjakan oleh subagent |

Yang disimpan untuk aksi tool hanya **ringkasan**: deskripsi dan perintah Bash, path file
untuk Read/Edit/Write, URL untuk WebFetch, dan sebagainya. Isi file yang ditulis, teks edit,
dan output tool yang sukses tidak disimpan.

### `login` / `sync` / `logout`

Menghubungkan mesin ini ke akun clioilc (backend di `../backend`), lalu meng-upload data
yang belum terkirim. Kontrak lengkapnya ada di repo backend: `docs/sync.md` (payload dan
protokol), `docs/auth.md` (device flow), dan `docs/encryption.md` (format e2e).

```sh
./target/release/clioilc login --server https://api.example.com   # sekali per mesin
./target/release/clioilc sync                                      # upload yang belum terkirim
./target/release/clioilc watch --sync                              # rekam + sync tiap 30 detik
./target/release/clioilc logout                                    # cabut token mesin ini
```

**`login`** memakai device flow. CLI menampilkan kode dan membuka halaman `/device` di
dashboard. Setelah kamu login dengan GitHub dan menyetujui kodenya, CLI menerima token.

| Opsi | Keterangan |
|---|---|
| `--server <URL>` | URL backend. Default `$CLIOILC_SERVER`, atau server dari login sebelumnya. Harus `https://`; `http://` hanya boleh untuk localhost |
| `--name <NAMA>` | nama perangkat di dashboard (default: hostname) |
| `--mode server\|e2e` | cara enkripsi data privat; ditanyakan saat login **pertama** akun, tidak bisa diganti |
| `--yes` | jangan bertanya sebelum membangun ulang `usage.db` (lihat di bawah) |
| `--no-browser` | jangan buka browser otomatis |

Setiap akun punya satu **salt bersama**, supaya project dan sesi yang sama menghasilkan hash
yang sama di semua laptop:

- **Perangkat pertama** meng-upload salt miliknya. Data lokalnya tetap berlaku.
- **Perangkat berikutnya** mengunduh salt akun. Kalau `usage.db`-nya kosong, salt langsung
  diganti. Kalau sudah berisi data, file itu dipindah ke `usage.db.before-login-<waktu>`,
  lalu dibangun ulang dari log agent dengan salt akun. Riwayat yang lognya sudah terhapus
  hanya tersisa di backup. Hentikan `clioilc watch` dulu sebelum login.

Mode enkripsi:

| Mode | Yang terjadi |
|---|---|
| `server` | data privat dikirim lewat HTTPS, lalu dienkripsi server saat disimpan |
| `e2e` | data privat dienkripsi di mesin ini dengan kunci dari passphrase (min. 12 karakter). Passphrase diperlukan di setiap perangkat baru dan di browser. Lupa passphrase berarti data privat hilang |

Untuk skrip atau CI, passphrase bisa diberikan lewat `CLIOILC_PASSPHRASE`.

**`sync`**:

- Mengirim batch berisi 500 baris sampai tidak ada yang tersisa.
- Baris yang berubah saat sedang dikirim (misalnya hasil tool baru masuk) tetap ditandai
  belum terkirim, dan ikut di batch berikutnya.
- Kalau upload gagal atau proses terhenti, baris dikembalikan ke status belum terkirim.
- Hanya satu proses sync yang bisa berjalan pada satu waktu (`sync.lock`).
- Sebelum upload, `sync` memastikan `usage.db` memakai salt akun. Kalau tidak, ia menolak
  dan meminta `login` ulang.

**`watch --sync`** menjalankan sync di background. Interval default 30 detik, bisa diatur
dengan `--sync-every <detik>`. Error yang sama hanya dicetak sekali. Supaya jalan otomatis
setiap kali login tanpa membuka terminal, pakai [`service install`](#service).

**`logout`** mencabut token mesin ini di server dan menghapus `auth.json`. Data lokal tetap
disimpan.

### `service`

Menjalankan `clioilc watch --sync` di background secara otomatis setiap kali login ke komputer,
dan menyalakannya lagi kalau berhenti. Di macOS memakai LaunchAgent (`launchd`), di Linux
memakai unit `systemd --user`.

```sh
./target/release/clioilc login --server https://api.example.com   # wajib dulu
./target/release/clioilc service install     # pasang dan langsung jalan
./target/release/clioilc service status      # running / not installed, plus 5 baris log terakhir
./target/release/clioilc service uninstall   # hentikan dan hapus
```

| | macOS | Linux |
|---|---|---|
| File | `~/Library/LaunchAgents/com.clioilc.watch.plist` | `~/.config/systemd/user/com.clioilc.watch.service` |
| Restart kalau berhenti | `KeepAlive`, jeda 30 detik | `Restart=always`, jeda 30 detik |
| Log | `<data dir>/watch.log` | `<data dir>/watch.log` |

Catatan:

- `install` menolak jalan kalau belum `login`. Tanpa login, watcher akan langsung berhenti dan
  terus dinyalakan ulang.
- Service menjalankan **binary yang dipakai saat `install`** (path absolut) dengan `--data-dir`
  yang sama. Kalau binary dipindah, jalankan `install` lagi.
- Hentikan dulu `clioilc watch` yang kamu jalankan manual, supaya tidak ada dua watcher sekaligus.
  Dua watcher tidak membuat data dobel, tapi bekerja dua kali.
- Saat service dihentikan (SIGTERM dari launchd/systemd), watcher berhenti dengan rapi seperti
  Ctrl+C: posisi baca tersimpan.
- `CLIOILC_SERVICE_LABEL` mengganti nama service, misalnya untuk akun atau data dir kedua.

### `add mod`

Membuat paket reader baru di `readers/<nama>` dan mendaftarkannya otomatis. Jalankan dari
folder mana saja di dalam workspace.

```sh
cargo run -q --release -- add mod cursor
# lalu edit readers/cursor/src/lib.rs, kemudian cargo build
```

Nama harus diawali huruf kecil, lalu hanya boleh berisi huruf kecil, angka, `-`, atau `_`.
Nama `core` tidak boleh dipakai.

## Format output `--follow`

Setiap baris di stdout berbentuk:

```
<path file lengkap><TAB><isi baris apa adanya>
```

Contoh (dipotong):

```
/Users/me/.claude/projects/-Volumes-SSD1-Private-clioilc-tracker/733d….jsonl	{"type":"assistant","message":{…},…}
```

- Bagian kedua adalah baris asli dari file, tanpa newline di akhir. Baris kosong dilewati.
- Karena JSON tidak pernah berisi karakter TAB mentah, TAB pertama pasti pemisah path dan isi.
- Baris yang belum selesai ditulis (belum ada `\n`) ditahan sampai lengkap, jadi `jq` tidak
  akan menerima JSON yang terpotong.

## Contoh pipeline

Semua contoh memakai `jq --unbuffered -R`. Alasannya ada di [Jebakan: buffering](#jebakan-buffering).

Ambil isi barisnya saja (buang path):

```sh
cargo run -q --release -- watch --follow \
  | jq --unbuffered -cR 'sub("^[^\t]*\t"; "") | fromjson'
```

Path dan isi sekaligus, misalnya untuk menampilkan nama project:

```sh
cargo run -q --release -- watch --follow \
  | jq --unbuffered -cR 'capture("^(?<path>[^\t]*)\t(?<line>.*)$")
      | (.line | fromjson) as $r
      | select($r.type == "assistant")
      | {project: (.path | split("/projects/")[1] | split("/")[0]),
         model: $r.message.model,
         output: $r.message.usage.output_tokens}'
```

Pantau satu project saja (filter berdasarkan path):

```sh
cargo run -q --release -- watch --follow \
  | grep --line-buffered -- '-Volumes-SSD1-Private-clioilc-tracker/' \
  | jq --unbuffered -cR 'sub("^[^\t]*\t"; "") | fromjson | .type'
```

Simpan ke file sambil tetap ditampilkan:

```sh
cargo run -q --release -- watch --follow | tee -a ~/claude-stream.tsv
```

## Membaca token Claude

### Struktur log Claude Code

File log ada di `~/.claude/projects/<project-slug>/<session-id>.jsonl`; log subagent ada di
`<session-id>/subagents/agent-*.jsonl`. Setiap baris adalah satu JSON object dengan field `type`:

| `type` | Isi yang berguna |
|---|---|
| `assistant` | `message.id`, `message.model`, `message.usage`, `message.content` (blok `text` / `thinking` / `tool_use`), `timestamp` |
| `user` | prompt pengguna (`text`) dan hasil tool (`tool_result`), `cwd`, `gitBranch` |
| `ai-title` | judul sesi |
| lainnya | metadata internal (`api-request`, `attachment`, `file-history-*`, …) |

Field di `message.usage`:

| Field | Arti |
|---|---|
| `input_tokens` | input baru yang tidak berasal dari cache |
| `cache_creation_input_tokens` | input yang ditulis ke cache |
| `cache_read_input_tokens` | input yang dibaca dari cache (biasanya paling besar: konteks percakapan) |
| `output_tokens` | token yang dihasilkan model |

### ⚠️ Satu balasan = beberapa baris

Claude Code menulis **satu baris per blok konten**. Satu balasan yang berisi thinking, text,
dan tool_use menjadi 3 baris `assistant` dengan `message.id` **sama** dan `usage` yang
**sama**. `stop_reason` juga sama di setiap baris, jadi tidak bisa dipakai untuk mencari
baris terakhir.

Akibatnya, menjumlahkan `usage` dari setiap baris akan **menghitung token 2–3 kali lipat**.
Contoh pertama di atas (`select(.type=="assistant") | .message.usage`) cocok untuk
*melihat* aliran token, tapi **jangan dijumlahkan langsung**.

Ambil satu baris per `message.id`:

```sh
cargo run -q --release -- watch --follow \
  | jq --unbuffered -cnR '
      foreach (inputs | sub("^[^\t]*\t"; "") | fromjson | select(.type == "assistant")) as $r
        ({}; .emit = (.[$r.message.id] == null) | .[$r.message.id] = true;
         select(.emit) | {id: $r.message.id, model: $r.message.model} + $r.message.usage)'
```

(`-n` + `inputs` + `foreach` menyimpan daftar id yang sudah dilihat selama pipeline berjalan.)

### Total token satu sesi yang sudah selesai

`--follow` hanya membaca baris baru. Untuk menghitung total dari file yang sudah ada, baca
filenya langsung:

```sh
jq -s '
  [ .[] | select(.type == "assistant") ]
  | unique_by(.message.id)
  | { messages: length,
      input:       (map(.message.usage.input_tokens // 0) | add),
      cache_write: (map(.message.usage.cache_creation_input_tokens // 0) | add),
      cache_read:  (map(.message.usage.cache_read_input_tokens // 0) | add),
      output:      (map(.message.usage.output_tokens // 0) | add) }' \
  ~/.claude/projects/-Volumes-SSD1-Private-clioilc-tracker/<session-id>.jsonl
```

`jq -s` memuat seluruh file ke memori. Untuk file berukuran ratusan MB itu masih bisa, tapi
butuh waktu beberapa detik.

### Privasi

Log berisi data sensitif: record `session-owner` memuat UUID akun dan organisasi, dan
`user`/`tool_result` bisa berisi isi file atau output perintah. Saring field-field ini
sebelum menyimpan atau mengirim hasil pipeline ke tempat lain.

## Jebakan: buffering

Pipeline ini **terlihat macet**, padahal datanya sudah masuk:

```sh
# ❌ output baru muncul setelah ~4 KB terkumpul
cargo run -q --release -- watch --follow | cut -f2- | jq -c '...'
```

Kebanyakan program Unix (`cut`, `sed`, `awk`, `grep`, `jq`) menahan output di buffer
(~4 KB) kalau stdout-nya adalah pipe, bukan terminal. clioilc sendiri melakukan flush setiap
batch. Yang menahan adalah program di tengah pipeline, dan `jq --unbuffered` tidak bisa
memperbaiki buffer milik `cut`.

Solusinya:

| Program | Cara agar realtime |
|---|---|
| `cut -f2-` | ganti dengan `jq -R 'sub("^[^\t]*\t"; "") \| fromjson'` |
| `jq` | `--unbuffered` (perlu kalau output `jq` di-pipe lagi atau ditulis ke file) |
| `grep` | `--line-buffered` |
| `sed` | `-l` (macOS) atau `-u` (GNU) |
| program lain (Linux) | `stdbuf -oL <program>` |

## Database dan privasi

`usage.db` memisahkan data menjadi tiga tingkat:

| Tingkat | Tabel | Isi | Siapa yang bisa lihat |
|---|---|---|---|
| **Publik** | `messages`, `tool_calls` | metadata: model, waktu, token, status, nama tool | boleh dibagikan ke publik |
| **Privat** | `private_projects`, `private_prompts`, `private_blocks`, `private_tool_calls` | nama project asli, prompt, balasan, thinking, ringkasan aksi tool, pesan error | hanya pemilik, setelah login di dashboard |
| **Lokal** | `meta` | salt untuk hash | tidak pernah keluar dari mesin ini |

Tabel publik dan privat memakai ID hash yang sama, jadi di dashboard pemilik bisa
menggabungkan metadata dengan isinya.

### Tingkat publik

| Disimpan | Tidak pernah disimpan |
|---|---|
| model, waktu (UTC, per detik), status ok/error, jumlah token | isi prompt, balasan, thinking, aksi tool |
| nama tool bawaan (`Bash`, `Edit`, …); tool MCP disimpan sebagai `mcp` saja | path file, `cwd`, nama branch, nama project asli |
| `is_subagent` | ID akun/organisasi, `requestId`, `uuid` |
| ID message, session, project, tool call: **di-hash** | ID mentah apa pun |

- **Hash:** SHA-256 dengan salt acak 32 byte yang dibuat sekali per mesin, dipotong menjadi
  128 bit. Hash-nya konsisten di mesin yang sama (dedupe tetap bekerja), tapi tidak bisa
  dicocokkan balik ke ID asli tanpa salt.
- **Label:** model, nama tool, dan jenis error hanya boleh berisi `A-Z a-z 0-9 _ - . < >` dan
  maksimal 64 karakter. Nilai lain diganti `unknown` / `other` / `api_error`, jadi teks bebas
  tidak mungkin masuk ke tabel publik.
- **Diverifikasi** pada log asli: tidak satu pun dari 17.029 ID mentah, slug project, path,
  atau potongan isi percakapan yang muncul di tabel publik.

### Tingkat privat

Sebelum disimpan, semua teks privat **disensor**, lalu **dipotong**:

- **Sensor rahasia:** API key (Anthropic, OpenAI, GitHub, GitLab, Slack, AWS, Google, Stripe),
  JWT, `Bearer` token, private key, password di URL (`postgres://user:****@host`), dan
  `password=` / `token=` / `api_key: …` diganti dengan `[REDACTED:<jenis>]`. Pada log asli,
  58 password database berhasil tersensor. Ini *best effort*: rahasia tanpa pola yang jelas
  (misalnya password yang diketik biasa di dalam kalimat) masih bisa lolos.
- **Batas panjang:** prompt, balasan, dan thinking maksimal 8.000 karakter; ringkasan tool 500;
  pesan error 1.000. Sensor dijalankan sebelum pemotongan, supaya rahasia tidak tersisa
  setengah.
- **Thinking:** hanya yang ada isinya. Sekitar 80% blok thinking di log memang kosong.

### Sync ke dashboard

- Tabel **publik** di-upload apa adanya.
- Tabel **privat** di-upload ke akun pemilik. Setiap user memilih cara perlindungannya:

| Mode | Cara kerja | Konsekuensi |
|---|---|---|
| **End-to-end** | dienkripsi di laptop sebelum dikirim; dashboard membuka isinya di browser dengan kunci/passphrase milik user | server tidak bisa membaca isinya; pencarian teks dilakukan di browser; kunci hilang berarti data privat hilang |
| **Enkripsi di server** | dikirim lewat HTTPS, server mengenkripsi saat disimpan dan membukanya untuk user yang login | lebih sederhana, pencarian bisa di server; tapi server bisa membaca isinya |

- Tabel **lokal** (`meta`) tidak pernah di-upload. Salt-nya tersimpan di akun: terenkripsi
  oleh server (mode server), atau dienkripsi dengan kunci E2E (mode e2e).

Format E2E (v1). Dashboard memakai format yang sama untuk dekripsi di browser:

- kunci passphrase = Argon2id(passphrase, `kdf.salt`, m=64 MiB, t=3, p=1), 32 byte
- data key: 32 byte acak, disimpan di server sebagai `seal(kunci passphrase, data key, "clioilc:v1:dek")`
- salt akun: `seal(data key, salt, "clioilc:v1:salt")`
- baris privat: `seal(data key, JSON data, "clioilc:v1:<kind>:<id>")`
- `seal` = `0x01` ‖ nonce 12 byte ‖ AES-256-GCM (ciphertext + tag), string tadi sebagai AAD

Test vector ada di `clioilc/src/cloud/e2e.rs` dan sudah dicek dengan argon2-cffi.
- Setiap baris yang di-sync punya kolom `synced_at` (`NULL` = belum di-upload), yang di-reset
  kalau barisnya berubah, misalnya hasil tool baru masuk. Primary key-nya hash yang stabil,
  jadi upload ulang bisa di-upsert dengan aman di sisi server.

```sh
sqlite3 ~/.clioilc/usage.db 'SELECT * FROM messages ORDER BY ts DESC LIMIT 5'
```

## File data

Semua ada di `--data-dir` (default `~/.clioilc`):

| File | Isi |
|---|---|
| `events.jsonl` | Satu event per baris: `timestamp`, `kind` (`Created`/`Modified`/`Removed`/`Renamed`), `path`, `reader`, `project`, `size` |
| `offsets.json` | Posisi baca `watch` per file (path, inode, offset). Kalau dihapus, `watch` mulai lagi dari akhir file. |
| `usage.db` | Token usage dan timeline aktivitas (SQLite), lihat [Database dan privasi](#database-dan-privasi) |
| `auth.json` | Hasil `login`: server, token perangkat, mode enkripsi, dan (mode e2e) data key. Permission 0600 |
| `sync.lock` | Mencegah dua `sync` berjalan bersamaan |
| `watch.log` | Output watcher yang dijalankan oleh `service install` (sync, error) |
| `usage.db.before-login-*` | Backup saat `login` membangun ulang `usage.db` dengan salt akun |

## Perilaku dan batasan

Cara `watch` membaca file besar:

- Setiap file punya offset dan handle yang tetap terbuka. File yang tidak berubah hanya
  memakan satu `stat`; file yang berubah hanya dibaca bagian barunya.
- Satu file dibaca maksimal 16 MB per putaran, supaya backlog besar di satu file tidak
  menunda file lain.
- Pada pengujian dengan file 208 MB dan burst 100 MB, memori puncaknya sekitar 3,4 MB.

Kasus khusus:

| Kejadian | Perilaku |
|---|---|
| File di-rename | posisi baca ikut pindah, tidak dibaca ulang |
| File di-truncate (ukurannya mengecil) | dibaca ulang dari awal |
| File diganti (inode berbeda, misalnya disimpan lewat temp file + rename) | dibaca ulang dari awal |
| File dihapus | handle langsung ditutup, supaya ruang disk dilepas |
| Baris lebih dari 64 MB | dilewati, dengan peringatan di stderr |
| Berhenti dengan Ctrl+C | posisi tersimpan tepat, tidak ada duplikat saat dijalankan lagi |
| Proses di-kill paksa (`kill -9`, crash) | baris dari maksimal 5 detik terakhir bisa terkirim ulang |
| Truncate lalu tumbuh lagi melewati offset lama sebelum terbaca | tidak terdeteksi (sama seperti `tail -F`) |
| Windows | tidak ada deteksi inode; file yang diganti hanya terdeteksi kalau ukurannya mengecil |

## Struktur workspace

```
Cargo.toml            workspace: versi, edition, dan dependency bersama
core/                 clioilc-core: trait Reader, Store, watcher, tail
  src/reader.rs       trait Reader (name, default_watch_dir, extensions, project_name)
  src/watcher.rs      loop notify, debounce event, integrasi --follow
  src/tail.rs         pembaca baris inkremental + checkpoint
  src/store.rs        events.jsonl
  src/filter.rs       EventFilter untuk history/summary
  src/usage.rs        tipe Record (Message, ToolResult, Prompt) hasil parsing reader
  src/usage_db.rs     SQLite tiga tingkat: hash, sanitasi, dedupe, report, activity
  src/redact.rs       sensor rahasia untuk teks privat
readers/claude/       reader ~/.claude/projects (project = folder pertama) + parser log
readers/gemini/       reader ~/.gemini
clioilc/              binary CLI
  src/readers.rs      daftar reader (diubah otomatis oleh `add mod`)
  src/commands/       watch, history, summary, usage, activity, add
```

Menjalankan test dan lint:

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
```
