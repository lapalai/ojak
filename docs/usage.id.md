# Panduan penggunaan Ojak

[Kembali ke README](README.id.md) · [English](usage.md) · [한국어](usage.ko.md) · **Bahasa Indonesia**

Mulai dari [unduhan dan penyiapan](README.id.md#unduh). Panduan ini membahas pemilihan akun, integrasi opsional, pembaruan, dan pemecahan masalah. Ojak masih dalam tahap pratinjau awal. Gunakan hanya akun milikmu dan baca [ketentuan penggunaan](README.id.md#ketentuan-penggunaan).

## Pemilihan akun

Ojak memilih akun saat **sesi baru yang dikelola dimulai**, bukan untuk setiap pesan. Percakapan yang sedang berjalan tetap memakai akun tersebut. Selain sisa kuota, Ojak mempertimbangkan status login, kebaruan data pemakaian, slot sesi, dan pengaturan pemilihan akun. Ojak tidak memesan kuota di sisi penyedia.

Jika terminal menampilkan `NO_ELIGIBLE_ACCOUNT` dan pesan bahwa CLI asli akan dijalankan, sesi itu **tidak dikelola Ojak** dan tidak dicatat sebagai sesi terkelola. Perilaku ini berlaku untuk perintah `claude`/`codex` yang terhubung ke Ojak; `aam run` tidak beralih ke CLI tanpa pengelolaan. Konflik autentikasi dan opsi yang tidak didukung juga tidak dilewati diam-diam.

Periksa alasannya tanpa memesan slot akun:

```sh
aam explain --tool claude
# Untuk Codex:
aam explain --tool codex
```

| Kode | Artinya |
|---|---|
| `QUOTA_EXHAUSTED` | Kuota yang berlaku untuk permintaan ini sudah habis. |
| `QUOTA_STALE` / `QUOTA_UNKNOWN` | Data pemakaian terlalu lama atau belum tersedia untuk pemilihan otomatis. Muat ulang data dan periksa login akun. |
| `CAPACITY_RESERVED` | Slot masih dipakai sesi lain, atau Ojak belum bisa memastikan sesi itu selesai. |
| `MODEL_LIMIT_GUARD` | Kuota khusus model sudah habis. Ini peringatan jika CLI yang memilih model; jika model tersebut diminta secara eksplisit, akun bisa tidak dipilih. |
| `RESERVE_DEPRIORITIZED` / `RESERVE_FALLBACK` | Sisa kuota sudah masuk cadangan pengaman. Ojak mendahulukan akun lain yang memenuhi syarat dan punya lebih banyak kuota, tetapi bisa memakai sisanya jika tidak ada pilihan lain. Ini penjelasan prioritas, bukan larangan mutlak. |

### Melanjutkan percakapan setelah kuota habis

Saat sesi interaktif yang dikelola Ojak berakhir setelah kuota habis, Ojak bisa menawarkan untuk membuka percakapan yang sama di akun lain yang tersedia. Setujui pertanyaan `[Y/n]` untuk melanjutkan. Jika data pemakaian belum diperbarui atau pertanyaannya tidak bisa ditampilkan, jalankan ini dari folder proyek yang sama:

```sh
aam continue
```

Ojak menyalin riwayat percakapan, bukan kredensial login. Percakapan dibuka lagi dalam proses baru, bukan dipindahkan tanpa jeda di dalam CLI yang sedang berjalan. Permintaan pertama bisa memakai lebih banyak token karena cache prompt dibuat ulang. Claude Code juga mungkin menampilkan panduan awal jika profil tersebut belum pernah dipakai secara interaktif.

### Pengaturan yang digunakan bersama

Kredensial akun tidak dibagikan antarprofil. Di macOS, Ojak menautkan folder ekstensi yang didukung dari `~/.claude` dan `~/.codex` ke profil terkelola, serta menyalin pengaturan tertentu. Di Windows, hanya pengaturan tertentu yang disalin; folder tidak ditautkan. Jangan menganggap semua folder skill dan plugin otomatis dipakai bersama di Windows. Daftar lengkapnya ada di [dokumentasi launcher](domains/launcher.md).

### Konflik autentikasi

`AUTH_OVERRIDE_CONFLICT` berarti ada API key, base URL khusus, auth helper, atau opsi lain yang bisa mengubah cara akun melakukan autentikasi. Ojak berhenti, bukan diam-diam mengubah konfigurasi. Periksa pengaturan yang disebutkan, tetapi jangan menempelkan nilai rahasianya ke issue. Kelola login lewat fitur akun Ojak, bukan lewat perintah yang mengubah autentikasi melalui shim terkelola.

## Integrasi omp

**Opsional dan tidak aktif secara bawaan.** Tampilan pemakaian dan sesi Claude Code/Codex yang dikelola tidak memerlukan bridge. Jika diaktifkan, kredensial langganan diteruskan melalui proxy lokal dan bisa bertentangan dengan ketentuan penyedia. Baca [ketentuan penggunaan](README.id.md#ketentuan-penggunaan) terlebih dahulu.

```sh
aam omp-broker connect && aam omp-bridge connect
```

Ojak mendaftarkan penyedia `ojak-*` sesuai login yang tersedia. Buka **sesi omp baru**, lalu pilih model `ojak-*` lewat `/model`. Permintaan melalui penyedia asli tidak memakai pemilihan akun Ojak.

- **Claude / Codex:** gunakan akun CLI resmi yang terdaftar di Ojak atau login penyedia asli di omp. Tidak perlu login dua kali.
- **Gemini (Antigravity), Grok, Z.AI:** gunakan login penyedia asli di omp.
- Akun yang ditambahkan belakangan biasanya terdeteksi dalam sekitar satu menit.
- Claude memakai keychain atau file kredensial aslinya; Codex memakai profil autentikasi berbasis file. CLI resmi menangani pembaruan autentikasi. Ojak tidak otomatis mengubah penyimpanan kredensial Codex lainnya menjadi penyimpanan file.

Detail implementasi tersedia di [dokumentasi bridge](domains/omp-bridge.md).

## Perkiraan biaya token dan tagihan penyedia

Halaman token dan biaya membaca catatan lokal omp, Claude Code, dan Codex untuk hari ini, bulan ini, dan seluruh periode. Perkiraan berdasarkan harga API **bukan tagihan langganan, saldo kredit, atau batas pengeluaran**. Model yang harganya belum diketahui ditampilkan terpisah. Pemindaian pertama bisa berlanjut di latar belakang setelah jendela ditutup; gambar karakter saat pemindaian bukan grafik pemakaian.

Dua pengaturan opsional memungkinkan Ojak memilih akun dengan **kredit Codex** atau **pemakaian tambahan Claude** setelah kuota langganan habis. Keduanya tidak aktif secara bawaan. Jika diaktifkan, akun yang memenuhi syarat dan masih punya kuota langganan tetap didahulukan. Setelah reset, sesi baru bisa kembali memakai kuota langganan.

Penyedia bisa mengenakan biaya. Ojak tidak bisa melihat jumlah yang ditagihkan atau mencegah CLI resmi yang sudah berjalan beralih ke pemakaian berbayar. Konfirmasi sebelum menjalankan CLI tanpa pengelolaan bukan jaminan untuk mencegah semua biaya. Dukungan saldo kredit bergantung pada data yang dilaporkan Codex. Alur kredit/pemakaian tambahan ini belum dikonfirmasi dengan akun berbayar sungguhan.

## Pembaruan

Gunakan pengecekan pembaruan di pengaturan aplikasi, bagian tentang Ojak. Build dengan kunci publik updater juga mengecek saat aplikasi dimulai dan setiap 24 jam, lalu menampilkan penanda pembaruan di popover menu bar/tray. Build tanpa kunci tersebut tidak menampilkan fitur pembaruan. Tanda tangan updater berbeda dari notarization Apple dan penandatanganan kode Windows.

**Selesaikan pekerjaan sebelum memperbarui.** Pemasangan pembaruan memulai ulang layanan Ojak dan bisa memutus sesi bridge omp sebentar. Jika ada sesi terkelola atau permintaan bridge baru-baru ini, aplikasi menawarkan pilihan memperbarui sekarang atau nanti. Installer Windows berhenti jika menemukan sesi aktif atau sesi yang belum dipastikan selesai.

Jika pembaruan dari aplikasi tidak tersedia, unduh DMG atau setup.exe baru dari [Releases](https://github.com/lapalai/ojak/releases/latest). Pergantian kunci tanda tangan bisa mengharuskan pemasangan manual, seperti pada transisi 0.1.0 → 0.1.1.

### Setelah memasang ulang secara manual

- **Mac:** mengganti `Ojak.app` belum tentu mengganti layanan yang sedang berjalan. Buka aplikasi dan gunakan tindakan mulai ulang layanan jika muncul peringatan versi berbeda. Mulai ulang yang aman akan ditolak saat sesi masih aktif atau belum dipastikan selesai, serta memberi peringatan jika omp baru saja digunakan. Dari CLI, gunakan `aam service restart`; periksa hasilnya lewat `aam service status` atau `aam setup --check` (`serviceVersionMismatch`).
- **Windows:** jalankan setup.exe dengan pengguna biasa yang sama. Installer memeriksa sesi, menghentikan layanan milik aplikasi, mengganti dan memverifikasi file, lalu menjalankan layanan kembali jika terdaftar untuk mulai saat login. Tutup Orca dan omp terlebih dahulu.

Khusus pengelola, layanan macOS bisa dimulai ulang langsung dengan `launchctl kickstart -k gui/$(id -u)/ai.aam.service`, dilanjutkan `aam integration install`. Berbeda dari tindakan mulai ulang yang aman, cara ini **tidak memeriksa sesi terkelola terlebih dahulu**.

## Pemasangan Windows

Jangan memasang atau menjalankan installer sebagai administrator. Pastikan installer berasal dari [rilis repositori ini](https://github.com/lapalai/ojak/releases/latest). Jika antivirus mengarantina `aam.exe` atau `aam-service.exe`, periksa deteksi dan sumber file sebelum mempertimbangkan pengecualian khusus untuk file itu. Jangan matikan perlindungan secara keseluruhan. Jika ragu, laporkan deteksinya tanpa kredensial atau data pribadi.

Jika installer menyatakan Ojak tidak bisa dihentikan dengan aman:

1. Selesaikan pekerjaan Claude Code/Codex, lalu **tutup Orca dan semua omp sepenuhnya**. Menutup jendela Orca saja bisa menyisakan observer yang mengunci `aam.exe`.
2. Jalankan installer lagi. Jika berhenti pada pemeriksaan keamanan ini, file aplikasi belum diganti.
3. Jika bagian detail installer menyebut `installer recover`, pemasangan sebelumnya mungkin terputus. Tutup Orca/omp dan [laporkan masalah](https://github.com/lapalai/ojak/issues), bukan mengedit database atau memaksa penggantian file.

Periksa detail installer dan `%TEMP%\Ojak-installer.log`. Mulai 0.6.2, kegagalan mencatat tahap (`stage`), hasil keluar helper (`exit`), dan kode kesalahan sebenarnya (`code`). Pesan umum versi lama tidak membuktikan bahwa pekerjaan aktif adalah penyebabnya. Tinjau dan samarkan data pribadi sebelum membagikan log.

- `SESSION_BUSY` / `INSTALLER_FILES_BUSY`: selesaikan pekerjaan aktif, tutup Orca/omp secara normal, lalu coba lagi.
- `OMP_CONFIG_FAILED`: pengaturan koneksi omp sebelumnya tidak dapat dipulihkan. Periksa pemasangan dan executable omp. Instalasi yang belum pernah terhubung ke omp dapat dihapus tanpa omp.
- `LEASE_STATE_UNKNOWN`: layanan berhenti sehingga keamanan sesi tersimpan belum dapat dipastikan. Jalankan kembali layanan Ojak dan periksa pekerjaan aktif; jangan hapus database atau lease secara manual.
- `HELPER_RESULT_UNAVAILABLE`: helper tidak menghasilkan file kode kesalahan. `exit=error` berarti gagal dijalankan; `exit=timeout` berarti nsExec kehabisan waktu. Periksa detail installer dan riwayat karantina antivirus.

## Slot sesi masih ditahan

Ojak tidak melepas slot hanya karena waktu sudah berlalu. Periksa status sesi:

- **`SUSPECT`:** hasil peluncuran atau status proses belum dipastikan. Ini sering teratasi saat launcher masih hidup.
- **`ORPHANED`:** pengawasan terputus sebelum launcher melaporkan penyelesaian. Proses turunannya mungkin masih ada.

Penyebabnya bisa berupa proses `claude -p` yang ditangguhkan atau launcher yang dihentikan dengan `kill -9`. Selesaikan tugas tersebut secara normal terlebih dahulu. Di **macOS**, periksa proses yang ditangguhkan dengan perintah berikut; `T` pada `STAT` berarti ditangguhkan:

```sh
ps -o pid,ppid,stat,command -ax | grep '[c]laude -p'
```

Hanya setelah memastikan proses itu milikmu dan memang ingin dihentikan, gunakan `kill -TERM <pid> && kill -CONT <pid>`. Jangan menyalin PID dari contoh orang lain atau menghentikan tugas yang tidak terkait. `CONT` membuat proses yang ditangguhkan bisa menangani sinyal penghentian. Jika launcher juga mati, laporan penyelesaiannya tetap bisa hilang.

Slot `ORPHANED` dengan catatan proses turunan bisa dilepas setelah proses turunan dan grup proses aslinya dipastikan selesai. Tanpa catatan itu, Ojak mungkin memerlukan restart komputer untuk memastikan tidak ada proses tersisa. Layanan bisa melepas slot setelah komputer dimulai ulang. **Jangan mengedit database layanan untuk membebaskan slot.** Akun lain yang tersedia tetap bisa menerima pekerjaan.

## Pemasangan macOS lanjutan

Baca [install.sh](../install.sh), lalu unduh ke folder kerja baru dan periksa isinya:

```sh
curl -fsSL https://raw.githubusercontent.com/lapalai/ojak/main/install.sh -o install.sh
```

Setelah ditinjau, lihat rencananya lalu jalankan:

```sh
sh install.sh --dry-run
sh install.sh
```

Skrip memilih rilis publik, mencocokkan DMG arm64 dengan `SHA256SUMS`, lalu memasang aplikasi di `~/Applications/Ojak.app`. Sebelum mengganti instalasi lama, pemeriksaan penghentian layanan harus berhasil. Sesi terkelola yang masih aktif atau belum dipastikan selesai akan menghalangi pembaruan. Hasil penyiapan diverifikasi dengan `aam setup --check`, bukan `--status`. Checksum bukan notarization Apple. Aplikasi yang sudah terpasang tetapi belum selesai disiapkan dilaporkan terpisah dari instalasi yang siap dipakai.

| Opsi | Efek |
|---|---|
| `--version 0.5.8` | Pilih rilis publik tertentu, bukan rilis terbaru. |
| `--dry-run` | Tampilkan rencana tanpa memasang atau mengubah pengaturan. |
| `--with-omp` | Hubungkan juga omp yang sudah terpasang; hanya jika diminta. Baca [risiko integrasi](#integrasi-omp). |
| `--system` | Pasang di `/Applications`; hanya mode pemasangan ini yang memakai sudo. |
| `--skip-service` | Lewati penyiapan layanan untuk instalasi baru atau versi yang sama. Opsi ini tidak dapat dipakai saat mengganti aplikasi lama karena layanan harus dihentikan dengan aman. |
| `--skip-integration` | Lewati `aam integration install`. |
| `--skip-shell` | Lewati `aam shell install`. |
| `--clear-quarantine` | Hapus atribut quarantine dari aplikasi yang disalin jika dipilih secara eksplisit. Tidak aktif secara bawaan dan bukan pengganti notarization. Utamakan alur izin macOS biasa. |

Gunakan `aam setup` untuk menyiapkan layanan dan integrasi terminal tanpa alur penyiapan desktop. Pengaturan PATH otomatis berlaku untuk zsh; untuk shell lain, ikuti petunjuk diagnostik penyiapan.

## Pemasangan lewat terminal Windows

Baca [install.ps1](../install.ps1), lalu unduh ke folder kerja baru dari PowerShell sebagai pengguna biasa:

```powershell
Invoke-WebRequest -UseBasicParsing https://raw.githubusercontent.com/lapalai/ojak/main/install.ps1 -OutFile install.ps1
```

Periksa isi skrip sebelum menjalankannya:

```powershell
.\install.ps1 -DryRun
.\install.ps1
```

Gunakan `-Version 0.5.8` untuk memilih rilis publik, `-WithOmp` untuk menghubungkan omp yang sudah terpasang, dan `-Help` untuk bantuan. Skrip memverifikasi checksum installer x64 lalu menjalankan installer NSIS yang tetap memeriksa sesi dan file terkunci. Tutup Orca dan omp sebelum memperbarui; agen tidak boleh memaksa sesinya sendiri berhenti. Jika kebijakan lokal memblokir skrip, gunakan setup.exe secara manual, bukan menonaktifkan kebijakan keamanan. Peringatan tentang installer tanpa tanda tangan tetap berlaku.

Untuk pemasangan lewat agen di kedua OS, gunakan [panduan bersama](install-for-agents.md). Persetujuan login mungkin tetap memerlukan tindakanmu. Jangan terus memanggil `aam setup --status` sambil menunggu `ready: true`: perintah itu tidak memverifikasi koneksi terminal.


## Menghapus aplikasi dan lokasi data

**Mac:** jalankan `aam deactivate` untuk membatalkan koneksi yang ditambahkan Ojak. Untuk menghapus aplikasinya juga, jalankan `sh install.sh --uninstall` dari salinan repositori atau folder berisi skrip instalasi yang sudah ditinjau. Gunakan `--uninstall --system` jika aplikasi dipasang di `/Applications`.

**Windows:** gunakan pengaturan aplikasi Windows untuk menghapus Ojak. Dalam antarmuka Inggris, jalurnya **Settings → Apps → Ojak → Uninstall**. Ojak memulihkan koneksi omp sebelum menghentikan layanan. Penghapusan berhenti jika masih ada sesi terkelola; selesaikan sesi atau mulai ulang PC, lalu coba lagi. `aam deactivate` juga memulihkan integrasi tanpa menghapus aplikasi.

Akun, profil, dan log tetap tersimpan setelah penghapusan:

| Platform | Direktori data Ojak |
|---|---|
| macOS | `~/Library/Application Support/AI Account Manager` |
| Windows | `%APPDATA%\AI Account Manager` |

Direktori ini berisi catatan akun/pemilihan dalam SQLite, `integration.json`, `ui-settings.json`, dan log. Kredensial tetap berada di penyimpanan atau profil CLI resmi terkait; jangan bagikan folder profil. Ojak tidak mengirim statistik pemakaian atau mengunggah diagnostik. Permintaan ke penyedia dan pengecekan pembaruan GitHub tetap memakai jaringan. Lihat [kebijakan keamanan](../SECURITY.md) untuk penanganan kredensial dan pelaporan privat.
