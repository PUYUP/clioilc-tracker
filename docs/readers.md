# Kontrak reader

Setiap reader menerjemahkan log satu tool AI (Claude Code, Devin, Gemini, dan lainnya) ke
format yang sama, yaitu daftar [`Record`](../core/src/usage.rs). Apa pun bentuk log aslinya,
yang keluar dari reader harus mengikuti kontrak di dokumen ini. Dengan begitu, journal,
dashboard, laporan biaya, dan sync ke server bekerja sama untuk semua tool tanpa kode khusus
per tool.

Kontrak ini **ditegakkan oleh kode**, bukan hanya ditulis:

- **`clioilc_core::contract::check`** memeriksa setiap record saat masuk ke `usage.db`
  (`UsageDb::insert`). Jalur log (`usage import`, `watch`) dan jalur database (Devin) sama-sama
  lewat gerbang ini.
- **Record yang akan merusak data ditolak**: dilewati, dihitung, dan dilaporkan.
- **Nilai yang masih bisa disimpan tetapi turun mutu** (misalnya nama model berisi spasi menjadi
  `unknown`) disimpan dengan nilai cadangan dan dilaporkan sebagai peringatan.
- **Tes setiap reader** memanggil `contract::assert_conforms`, yang gagal untuk pelanggaran apa
  pun, termasuk peringatan.

Contoh laporan di terminal saat `usage import` atau `watch`:

```
[cursor] skipped 3 record(s) that break the reader contract (docs/readers.md): message.id is empty ×3
[cursor] stored with a fallback: message.model is not a label (stored as 'unknown') ×12
```

## Alur data

```
log / database tool ──► Reader ──► Record ──► contract::check ──► UsageDb ──► sync ──► server
                        (per tool)  (format     (tolak / peringatan) (hash, label,  (validasi
                                     bersama)                        sensor, potong)  ulang)
```

Pembagian tugasnya:

| Tahap | Tanggung jawab |
|---|---|
| **Reader** | Membaca format tool, lalu mengisi `Record` dengan nilai **mentah** apa adanya, kecuali pemetaan yang diwajibkan di bawah (nama tool, nama proyek). |
| **`contract::check`** | Menolak record yang rusak, menghitung nilai yang turun mutu. |
| **`UsageDb`** | Meng-hash semua id, menyaring label, menyensor rahasia, memotong teks panjang, membuang nilai yang akan ditolak server. **Reader tidak boleh melakukan ini sendiri**, karena hash butuh salt lokal dan sensor harus seragam. |
| **Server** (`POST /api/v1/usage/sync/`) | Memvalidasi ulang dengan aturan yang sama (`backend/apps/usage/api/v1/serializers.py`). |

Karena hashing dan penyensoran ada di `UsageDb`, reader tidak bisa membocorkan data privat ke
tabel publik secara tidak sengaja: field privat hanya pernah masuk ke tabel `private_*`.

## Dua jenis reader

| Jenis | Yang diimplementasikan | Contoh |
|---|---|---|
| **Log** (file yang terus bertambah) | `Reader::parse_line(line) -> Vec<Record>` dan `Reader::project_name(path)` | `claude` |
| **Database** (riwayat di SQLite atau sejenisnya) | `Reader::database() -> Some(&dyn DatabaseSource)` dengan `read_since(path, cursor, out)` | `devin` |

Untuk keduanya:

- **Satu masukan boleh menghasilkan nol, satu, atau beberapa record.** Baris yang tidak relevan
  menghasilkan `Vec` kosong, dan ini harus murah karena kebanyakan baris log memang tidak
  relevan.
- **Membaca ulang harus aman.** `usage import` bisa diulang kapan saja, jadi id yang sama harus
  selalu keluar untuk data yang sama (lihat [Id dan dedupe](#id-dan-dedupe)).
- **Database milik tool dibuka read-only.**
- **Format yang tidak dikenali tidak boleh dilewati diam-diam.** Hitung lalu laporkan, seperti
  reader `devin` (`N message(s) in an unknown format were skipped`).

## Record

Ada tiga jenis record. Kolom **Tingkat** menunjukkan ke mana nilainya disimpan:

- **publik**: boleh dibagikan, hanya metadata;
- **privat**: hanya pemilik, terenkripsi saat sync;
- **lokal**: tidak pernah keluar dari laptop.

### `Record::Message`: satu respons model

| Field | Wajib | Aturan | Tingkat |
|---|---|---|---|
| `id` | ya | Id respons dari provider. **Sama** untuk semua baris dari satu respons (Claude menulis satu baris per blok konten) dan untuk salinan respons yang sama (fork di Devin). Menjadi kunci dedupe: token disimpan dengan nilai terbesar, bukan dijumlah. | publik (hash) |
| `line_id` | ya | Id baris atau node ini. Membedakan blok-blok dari satu respons. | dipakai untuk id blok privat |
| `session_id` | ya | Id percakapan. Subagent memakai id sesi induknya. | publik (hash) |
| `is_subagent` | ya | `true` untuk respons subagent (Task/Agent). | publik |
| `timestamp` | ya | Waktu respons, dalam UTC. Antara 2020 dan besok. | publik (detik), privat (milidetik) |
| `model` | ya | Id model persis seperti dari provider, misalnya `claude-opus-5-5` atau `swe-2-flash-medium`. Harus berupa **label**. Jangan diganti dengan nama tampilan seperti "Opus 5.5", karena harga dicocokkan lewat id ini (`ModelPrice`). | publik |
| `status` | ya | `Ok`, atau `Error(kind)` dengan `kind` berupa label yang bisa dibaca mesin, misalnya `rate_limit`, `overloaded`, atau `api_error`. | publik |
| `usage.input` | ya | Token input **baru**, tidak termasuk cache read maupun cache write. | publik |
| `usage.cache_write` | ya | Token yang ditulis ke cache prompt. Isi 0 kalau tool tidak punya cache. | publik |
| `usage.cache_read` | ya | Token yang dibaca dari cache prompt. | publik |
| `usage.output` | ya | Token output, termasuk thinking. | publik |
| `usage.cache_write_5m`, `usage.cache_write_1h` | tidak | Rincian `cache_write` per durasi cache. Isi **keduanya atau tidak sama sekali**, dan jumlahnya harus sama dengan `cache_write`. Rincian yang tidak cocok dibuang. | publik |
| `speed` | tidak | Label kecepatan layanan, misalnya `standard` atau `fast` (harganya lebih tinggi). | publik |
| `blocks` | tidak | Teks yang terlihat dan thinking di baris ini, masing-masing `{kind: Text \| Thinking, text}`. | privat |
| `tool_calls` | tidak | Pemanggilan tool di baris ini (lihat di bawah). | campuran |
| `branch` | tidak | Branch git tempat agent bekerja, kalau tool mencatatnya. | lokal |

**Satuan token.** Biaya dihitung server sebagai `token × harga per token` untuk keempat jenis
token. Jadi pastikan `input` tidak ikut menghitung token cache. Kalau tool hanya memberi angka
total, kurangi bagian cache-nya dulu.

### `ToolCall` (di dalam `Message`)

| Field | Wajib | Aturan | Tingkat |
|---|---|---|---|
| `id` | ya | Id pemanggilan, sama dengan `ToolResult.tool_call_id` hasilnya. | publik (hash) |
| `name` | ya | **Nama kanonik** (lihat [Nama tool](#nama-tool)). Harus berupa label, dan tidak boleh memuat nama privat seperti `mcp__server__tool` (ganti dengan `mcp`). | publik |
| `full_name` | ya | Nama asli dari tool, misalnya `exec` atau `mcp__acme_crm__lookup`. | privat |
| `summary` | tidak | Ringkasan aksi (lihat [Isi `summary`](#isi-summary)). Jangan memuat isi file atau diff. | privat |
| `git_dirs` | tidak | Untuk perintah shell yang membuat commit: folder repositori, dari `clioilc_core::git::commit_dirs(command, cwd)`. | lokal |

### `Record::ToolResult`: hasil sebuah pemanggilan tool

| Field | Wajib | Aturan | Tingkat |
|---|---|---|---|
| `tool_call_id` | ya | Id pemanggilan yang hasilnya ini. Boleh datang sebelum atau sesudah `Message`-nya. | publik (hash) |
| `is_error` | ya | `true` kalau aksinya gagal: tool melaporkan error, **atau perintah shell keluar dengan kode bukan 0**. | publik (`ok` atau `error`) |
| `error_text` | tidak | Output error, hanya kalau `is_error`. Diabaikan untuk aksi yang berhasil. | privat |
| `timestamp` | tidak | Kapan aksi selesai. Dipakai untuk mencari commit di rentang waktu perintah. | lokal |
| `commits` | tidak | Commit yang **diumumkan output-nya**, misalnya `[main 3c514b0] Subject`. Parse dengan `clioilc_core::git` dan jangan percayai output dari aksi yang gagal. `hash` harus 7 sampai 64 karakter hex huruf kecil; yang tidak valid dibuang. | privat |
| `prs` | tidak | URL pull atau merge request yang dibuat aksi ini (`gh pr create`, `glab mr create`), bukan yang sekadar disebut. Harus berupa URL `http(s)` dengan panjang maksimal 500. | privat |

### `Record::Prompt`: yang diketik manusia

| Field | Wajib | Aturan | Tingkat |
|---|---|---|---|
| `id` | ya | Id baris atau node prompt, untuk dedupe. | privat (hash) |
| `session_id` | ya | Sama dengan `Message.session_id` di sesi itu. | privat (hash) |
| `is_subagent` | ya | `true` untuk instruksi yang dikirim ke subagent. | privat |
| `timestamp` | ya | Dalam UTC, antara 2020 dan besok. | privat |
| `text` | ya | Teks yang diketik **manusia**, tanpa konteks yang disisipkan tool (system reminder, isi file yang ditempel otomatis, ringkasan compact). Jangan keluarkan record untuk teks kosong. | privat |
| `branch` | tidak | Seperti di `Message`. | lokal |

Pesan yang dibuat tool sendiri, misalnya hasil tool atau ringkasan setelah compact, **bukan**
prompt. Journal memecah pekerjaan per prompt, jadi prompt palsu akan memecah satu pekerjaan
menjadi banyak.

### Proyek

Proyek tidak ada di dalam `Record`. Proyek dikirim terpisah: hasil `project_name(path)` untuk
reader log, atau argumen pertama `out(project, record)` untuk reader database.

- **Pakai nama gaya Claude Code untuk folder kerja**: path absolut dengan setiap karakter di luar
  `[A-Za-z0-9-]` diganti `-`, jadi `/Users/me/app` menjadi `-Users-me-app`. Dengan begitu, folder
  yang dikerjakan beberapa agent menjadi satu proyek di dashboard.
- **Proyek kosong atau tidak diketahui disimpan sebagai `-`** dan memicu peringatan. Panjang
  maksimal 1000 karakter.
- **Nama proyek bersifat privat.** Tabel publik hanya berisi hash-nya.

## Nama tool

`ToolCall.name` adalah kosakata bersama. Journal (`core/src/journal.rs`) dan dashboard
(`frontend/src/lib/digest.js`) mengenali edit, commit, dan push dari nama-nama ini, jadi
**aksi yang setara wajib memakai nama yang sama**, apa pun nama aslinya di tool:

| Aksi | `name` | Dipakai untuk |
|---|---|---|
| Menjalankan perintah shell | `Bash` | Deteksi commit, push, dan PR; daftar perintah per prompt |
| Mengubah sebagian file | `Edit` | File yang diubah, file yang belum di-commit |
| Menulis file utuh | `Write` | Sama dengan `Edit` |
| Beberapa edit sekaligus | `MultiEdit` | Sama dengan `Edit` |
| Edit notebook | `NotebookEdit` | Sama dengan `Edit` |
| Membaca file | `Read` | |
| Mencari teks | `Grep` | |
| Mencari file berdasarkan nama | `Glob` | |
| Mengambil URL | `WebFetch` | |
| Pencarian web | `WebSearch` | |
| Daftar tugas | `TodoWrite` | |
| Bertanya ke pengguna | `AskUserQuestion` | |
| Menjalankan skill | `Skill` | |
| Menjalankan subagent | `Agent` (`Task` di Claude Code versi lama) | |
| Tool dari server MCP | `mcp` | Nama server MCP bisa membuka integrasi privat |
| Lainnya | nama asli tool, kalau berupa label | Disimpan sebagai `other` kalau bukan label |

Contoh pemetaan di reader `devin` (`claude_name`): `exec`→`Bash`, `edit`→`Edit`,
`write`→`Write`, `read`→`Read`, `grep`→`Grep`, `find_file_by_name`→`Glob`.

### Isi `summary`

Satu ringkasan pendek dari aksi, karena journal membaca isinya:

| `name` | `summary` |
|---|---|
| `Bash` | Perintahnya, boleh diawali deskripsi (`deskripsi: perintah`). Journal mencari `git commit`, `git push`, `gh pr create` di sini. |
| `Edit`, `Write`, `MultiEdit`, `NotebookEdit` | **Path absolut file** saja. Journal memakainya sebagai daftar file yang diubah. |
| `Read` | Path file. |
| `Grep`, `Glob` | Pola, opsional `: path`. |
| `WebFetch` | URL. |
| `WebSearch` | Kata kunci. |
| Lainnya | Satu atau dua argumen yang paling menjelaskan aksi. |

Isi file, isi edit (teks lama atau baru), dan output perintah tidak termasuk `summary`. Teks
dipotong menjadi 500 karakter, dan rahasia seperti token atau password disensor oleh `UsageDb`.

## Id dan dedupe

| Record | Kunci dedupe | Kalau datang lagi |
|---|---|---|
| `Message` | `id` | Token mengambil nilai terbesar dari yang tersimpan dan yang baru. Respons yang ditulis bertahap tetap benar, dan salinan tidak terhitung dua kali. |
| Blok teks | `line_id` dan urutan blok | Diabaikan. |
| `ToolCall` | `id` | Nama dan pesan induknya diisi sekali. |
| `ToolResult` | `tool_call_id` | Status diperbarui. |
| `Prompt` | `id` | Diabaikan. |

Id **harus stabil**: data yang sama selalu menghasilkan id yang sama, juga saat dibaca ulang
dari awal. Jangan memakai nomor urut buatan reader atau waktu saat membaca. Id kosong ditolak,
karena semua record dengan id kosong akan jatuh ke satu hash yang sama dan menyatu.

## Aturan yang diperiksa

Daftar ini sama persis dengan teks di [`core/src/contract.rs`](../core/src/contract.rs).

**Ditolak** (record dilewati):

| Aturan | Alasan |
|---|---|
| `reader name is not a label` | Nama reader menjadi kolom `tool` di tabel publik. |
| `message.id is empty` | Semua respons akan menyatu menjadi satu. |
| `message.line_id is empty` | Blok dari baris yang berbeda akan saling menimpa. |
| `message.session_id is empty` | Semua sesi akan menyatu menjadi satu. |
| `message.timestamp is before 2020 or in the future` | Data akan jatuh di luar semua laporan. |
| `message.tool_calls[].id is empty` | Hasil tool tidak bisa dicocokkan. |
| `tool_result.tool_call_id is empty` | Sama seperti di atas. |
| `prompt.id is empty` | Prompt akan menyatu menjadi satu. |
| `prompt.session_id is empty` | Prompt tidak bisa dikaitkan ke sesinya. |
| `prompt.timestamp is before 2020 or in the future` | Data akan jatuh di luar semua laporan. |

**Peringatan** (disimpan dengan nilai cadangan, atau bagian itu dibuang):

| Aturan | Akibat |
|---|---|
| `project is missing (stored as '-')` | Data masuk ke proyek `-`. |
| `project is empty` | Sama seperti di atas. |
| `project is longer than 1000 characters (name cut)` | Nama dipotong. |
| `message.model is not a label (stored as 'unknown')` | Biaya tidak bisa dihitung. |
| `message.status error kind is not a label (stored as 'api_error')` | Jenis error hilang. |
| `message.speed is not a label (stored as 'unknown')` | Dihargai dengan tarif standar. |
| `message.usage cache split does not add up to cache_write (split dropped)` | Semua cache write dihargai dengan tarif 1 jam. |
| `message.blocks has an empty text (stored empty)` | Ada blok kosong di Activity. |
| `message.tool_calls[].name is not a label (stored as 'other')` | Journal tidak mengenali aksinya. |
| `message.tool_calls[].name looks like a private name (map it to 'mcp')` | Nama server MCP terlihat di tabel publik. |
| `message.tool_calls[].full_name is empty` | Nama asli tool tidak ada di Activity. |
| `tool_result.timestamp is before 2020 or in the future (ignored)` | Pencarian commit memakai rentang default. |
| `tool_result.error_text is set on a success (ignored)` | Diabaikan. |
| `tool_result.commits[].hash is not 7-64 lowercase hex (commit dropped)` | Commit itu tidak tercatat. |
| `tool_result.prs[] is not an http(s) URL of at most 500 characters (dropped)` | PR itu tidak tercatat. |
| `prompt.text is empty` | Prompt kosong di Activity. |

**Label** adalah 1 sampai 64 karakter dari `A-Z a-z 0-9 _ - . < >`. Server memakai pola yang
sama (`LABEL_RE`), jadi nilai yang bukan label tidak pernah sampai ke tabel publik.

## Yang ditangani otomatis

Bagian berikut **tidak perlu** dan **tidak boleh** dikerjakan reader:

| Hal | Dikerjakan oleh |
|---|---|
| Meng-hash id, sesi, dan proyek (dengan salt lokal) | `UsageDb::hash` |
| Menyaring label (`model`, `status`, `name`, `speed`) | `UsageDb` dengan `contract::is_label` |
| Menyensor rahasia di teks privat | `redact::redact` |
| Memotong teks: prompt, balasan, dan thinking 8.000 karakter; `summary` 500; error 1.000; `full_name` 128 | `UsageDb` |
| Melengkapi commit dari repositori (hash penuh, branch, remote, file) | `UsageDb::resolve_commits` dengan `git::resolve` |
| Menyusun work item untuk journal | `UsageDb::build_work_items` |
| Menghitung biaya | Server, dari `ModelPrice` dan token per pesan |

## Menambah reader

1. `clioilc add mod <nama>` membuat `readers/<nama>` dari template, lengkap dengan tes kontrak.
2. Implementasikan `parse_line` dan `project_name` untuk tool berbasis log, atau `database()`
   untuk tool yang menyimpan riwayat di database.
3. Petakan nama tool ke [nama kanonik](#nama-tool) dan isi `summary` sesuai tabelnya.
4. Tempel beberapa baris atau row **asli** dari tool ke tes `records_follow_the_contract`:
   satu respons dengan tool call, satu hasil tool yang gagal, satu prompt, dan satu baris yang
   harus diabaikan. Panggil `contract::assert_conforms` pada hasilnya.
5. Uji dengan data nyata di folder terpisah supaya `~/.clioilc` tidak tersentuh:

   ```sh
   cargo run -q --release -- --data-dir /tmp/clioilc-test usage import --reader <nama>
   ```

   Tidak boleh ada baris `skipped … break the reader contract` atau `stored with a fallback`.
6. Cek `clioilc activity` dan `clioilc journal`: prompt, balasan, edit, dan commit harus muncul
   seperti untuk Claude Code.

## Lihat juga

- [`core/src/usage.rs`](../core/src/usage.rs): definisi `Record`.
- [`core/src/contract.rs`](../core/src/contract.rs): aturan yang diperiksa.
- [`core/src/usage_db.rs`](../core/src/usage_db.rs): tabel publik, privat, dan lokal.
- `backend/docs/sync.md` dan `backend/docs/api-v1.md`: format sync dan validasi di server.
