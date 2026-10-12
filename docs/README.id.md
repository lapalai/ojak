<p align="center">
  <img src="../apps/desktop/src/assets/ojak-icon.png" width="96" height="96" alt="Ojak — burung magpie di atas jembatan">
</p>

<h1 align="center">Ojak</h1>

<p align="center"><strong>Akun Claude Code dan Codex kamu, di satu tempat.</strong></p>

<p align="center">
  <a href="https://github.com/lapalai/ojak/releases/latest"><strong>Unduh</strong></a> ·
  <a href="#mulai-pakai-ojak">Mulai pakai</a> ·
  <a href="usage.id.md">Panduan</a> ·
  <a href="https://github.com/lapalai/ojak/issues">Laporkan masalah</a>
</p>

<p align="center">
  <a href="../README.md">English</a> · <a href="README.ko.md">한국어</a> · <strong>Bahasa Indonesia</strong>
</p>

<p align="center">
  <a href="https://github.com/lapalai/ojak/releases/latest"><img src="https://img.shields.io/github/v/release/lapalai/ojak" alt="Rilis terbaru"></a>
  <a href="https://github.com/lapalai/ojak/actions/workflows/ci.yml"><img src="https://github.com/lapalai/ojak/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="../LICENSE"><img src="https://img.shields.io/github/license/lapalai/ojak" alt="Lisensi MIT"></a>
</p>

Ojak adalah **aplikasi menu bar macOS dan system tray Windows** untuk mengelola akun langganan Claude Code dan Codex milikmu sendiri. Lihat sisa kuota dan waktu reset tiap akun dalam satu layar, lalu jalankan `claude` atau `codex` seperti biasa. Setelah perintah terminal terhubung, Ojak memilih akun yang bisa digunakan untuk setiap sesi baru.

**Gratis dan open source.** Pasang CLI resmi Claude Code atau Codex terlebih dahulu, lalu hubungkan akunmu sendiri. Langganan dan biaya pemakaian dari penyedia tetap terpisah.

> **Masih dalam tahap pratinjau awal.** Cara kerja dan langkah pemasangan bisa berubah. Ojak adalah proyek tidak resmi, tanpa afiliasi dengan Anthropic, OpenAI, Google, atau xAI. Gunakan hanya akun milikmu. Ojak tidak menambah kuota atau mengakali batas pemakaian. [Baca ketentuan penggunaan](#ketentuan-penggunaan).

## Kenapa pakai Ojak?

- **Cek kuota tanpa berpindah akun.** Sisa kuota dan waktu reset tiap akun tersedia dalam satu layar.
- **Tetap pakai terminal seperti biasa.** CLI resminya tetap sama, begitu juga perintah `claude` dan `codex`.
- **Lanjutkan otomatis setelah kuota habis.** Pada sesi interaktif yang didukung, Ojak mendeteksi catatan kehabisan kuota, memastikan proses lama sudah berhenti, lalu membuka percakapan yang sama di akun lain yang tersedia. [Cakupan dan batasan](usage.id.md#melanjutkan-percakapan-setelah-kuota-habis).
- **Pahami pemakaian token.** Lihat jumlah token dan perkiraan biaya berdasarkan harga API dari catatan omp, Claude Code, dan Codex di komputer ini. Angkanya bukan tagihan langgananmu; model yang harganya belum diketahui ditandai terpisah.

Antarmuka aplikasi tersedia dalam **Bahasa Indonesia, Inggris, dan Korea**. Sebagian pesan CLI, layanan, dan installer masih berbahasa Korea.

## Minta agen AI memasang Ojak

Tempel permintaan ini ke Claude Code, Codex, atau omp yang bisa menjalankan perintah lokal:

```text
Pasang dan siapkan Ojak untuk komputer saya mengikuti panduan ini. Pertahankan pengaturan dan login yang ada, lalu verifikasi hasilnya:
https://raw.githubusercontent.com/lapalai/ojak/main/docs/install-for-agents.md
```

Untuk menghubungkan omp juga, tambahkan “Hubungkan juga omp yang sudah terpasang.” Integrasi ini opsional dan tidak aktif secara bawaan; baca [ketentuan penggunaan](#ketentuan-penggunaan). Persetujuan login dan izin keamanan OS mungkin tetap memerlukan tindakanmu. Agen tidak boleh memaksa tugas yang berjalan berhenti demi pembaruan.

Ingin memasang sendiri? Ikuti petunjuk terminal untuk [Mac](usage.id.md#pemasangan-macos-lanjutan) atau [Windows](usage.id.md#pemasangan-lewat-terminal-windows), atau unduh installer di bawah. [Panduan untuk agen](install-for-agents.md).

## Unduh

Ambil installer dari [rilis terbaru](https://github.com/lapalai/ojak/releases/latest). Tautan ini tetap sama saat ada versi baru.

| Platform | File yang diunduh | Persyaratan |
|---|---|---|
| macOS | `Ojak_<versi>_aarch64.dmg` | macOS 13 ke atas, Apple silicon (seri M) |
| Windows (pratinjau) | `Ojak_<versi>_x64-setup.exe` | Windows 10 1809 ke atas atau Windows 11, x64; perlu WebView2 |

Belum ada build untuk Mac Intel atau Linux. WebView2 sudah disertakan di Windows 11; di Windows 10, kamu mungkin perlu memasangnya secara terpisah.

### macOS

1. Buka DMG, lalu seret **Ojak** ke folder **Applications**.
2. Buka Ojak. Aplikasi ini memakai tanda tangan ad-hoc dan belum mendapat notarization dari Apple. Jika macOS memblokirnya karena pengembang tidak dikenal, pastikan file berasal dari repositori ini sebelum mengizinkannya lewat pengaturan privasi dan keamanan. Dalam antarmuka berbahasa Inggris, jalurnya adalah **System Settings → Privacy & Security → Open Anyway**.
3. Ikuti langkah penyiapan di bawah. macOS bisa meminta izin lagi setelah pembaruan.

### Windows

1. Jika kamu memakai Orca atau omp, **tutup aplikasinya sepenuhnya sebelum memasang atau memperbarui Ojak**. Menutup jendela Orca saja bisa membuat omp tetap berjalan.
2. Jalankan installer dengan pengguna Windows yang biasa kamu pakai. **Jangan pilih “Run as administrator.”**
3. Installer ini belum memiliki tanda tangan digital. Jika SmartScreen muncul, periksa sumber unduhannya sebelum melanjutkan. Pada Windows berbahasa Inggris, pilih **More info → Run anyway**. Jangan matikan antivirus. Jika ada file yang diblokir atau pemasangan berhenti, baca [panduan masalah pemasangan](usage.id.md#pemasangan-windows).

## Mulai pakai Ojak

1. **Buka Ojak dan pilih Mulai.** Langkah ini menyiapkan layanan latar belakang dan menghubungkan perintah terminal.
2. **Hubungkan akunmu.** Ojak mencari login Claude Code dan Codex yang sudah ada di komputer ini. Jika diperlukan, gunakan tombol login di aplikasi untuk membuka halaman login resmi penyedia. Untuk menambah akun, buka halaman koneksi akun.
3. **Buka terminal baru** di folder proyekmu, lalu jalankan salah satu perintah ini:

   ```sh
   claude
   # atau
   codex
   ```

   Di Mac, penyiapan bisa membuka terminal dengan perintah yang sudah terisi; tekan Enter untuk menjalankannya. Di Windows, salin perintah ke terminal baru.

4. **Lihat halaman sesi** untuk mengetahui akun yang dipilih.

Kalau penyiapan belum selesai, kartu panduan awal menunjukkan langkah yang tersisa. Jika tidak ada akun yang bisa dipilih, jangan berasumsi sesinya dikelola Ojak: [periksa pesan CLI dan alasan akun tidak dipilih](usage.id.md#pemilihan-akun).

**Sudah pakai omp?** Integrasinya opsional dan tidak aktif secara bawaan. Pemilihan akun untuk CLI tidak memerlukan omp. Integrasi ini meneruskan kredensial langganan melalui proxy lokal dan bisa bertentangan dengan ketentuan penyedia. Baca [cara menghubungkan dan risikonya](usage.id.md#integrasi-omp) sebelum mengaktifkannya.

## Privasi dan keamanan akun

Ojak menyimpan catatan akun, pengaturan, dan log diagnostik di komputermu. **Ojak tidak mengirim statistik pemakaian atau mengunggah diagnostik secara otomatis.** Ini bukan berarti aplikasinya bekerja secara offline: login, pengecekan kuota, dan permintaan model tetap terhubung ke penyedia. Pengecekan pembaruan menghubungi GitHub.

- Akun dipilih saat sesi baru dimulai. Sesi interaktif yang didukung dapat dibuka kembali secara otomatis di akun lain setelah kuota dipastikan habis; proses lama dihentikan terlebih dahulu, bukan mengganti kredensial di dalam CLI yang sedang berjalan.
- Jika Ojak belum bisa memastikan suatu sesi sudah selesai, slot akun tetap ditahan agar tidak dipakai oleh sesi lain secara bersamaan tanpa kepastian.
- Perkiraan biaya token bukan tagihan. Fitur opsional kredit Codex dan pemakaian tambahan Claude bisa menimbulkan biaya dari penyedia; penggunaannya untuk sesi baru tidak aktif secara bawaan. Ojak tidak bisa membatasi biaya dari CLI resmi yang sudah berjalan.

Lihat [lokasi data, penjelasan biaya, dan pemecahan masalah](usage.id.md), serta [kebijakan keamanan](../SECURITY.md) (Inggris/Korea).

## Ketentuan penggunaan

Gunakan hanya akun milikmu sendiri. Jangan gunakan Ojak untuk berbagi akun atau menghindari batas pemakaian. Kamu bertanggung jawab mematuhi ketentuan penyedia; akun bisa dibatasi atau ditangguhkan. Proyek ini tidak menjamin bahwa suatu cara penggunaan diizinkan atau bebas risiko.

Integrasi omp yang opsional meneruskan kredensial langganan melalui proxy lokal dan bisa bertentangan dengan ketentuan penyedia. Baca [informasi hukum dan kepatuhan Anthropic](https://code.claude.com/docs/en/legal-and-compliance) dan [ketentuan penggunaan OpenAI](https://openai.com/policies/row-terms-of-use/).

## Dokumentasi dan kontribusi

- **Panduan penggunaan:** [Bahasa Indonesia](usage.id.md) · [English](usage.md) · [한국어](usage.ko.md)
- **Perubahan terbaru:** [Changelog](../CHANGELOG.md) (Korea)
- **Build dan kontribusi:** [Panduan kontribusi](../CONTRIBUTING.md) (Inggris/Korea)
- **Arsitektur:** [Indeks domain](domains/INDEX.md) · [Kebijakan rilis](release-policy.md) (Korea)
- **Komunitas:** [Kode etik](../CODE_OF_CONDUCT.md) · [Laporkan masalah keamanan secara privat](../SECURITY.md)

Untuk membuat build sendiri, ikuti persyaratan di [panduan kontribusi](../CONTRIBUTING.md), lalu jalankan `npm ci` dan `npm run build` dari direktori utama repositori.

## Asal nama Ojak

Nama Ojak diambil dari **Ojakgyo**, jembatan burung gagak dan magpie dalam cerita rakyat Korea. Setahun sekali, burung-burung itu membentuk jembatan di Bima Sakti agar dua kekasih yang terpisah bisa bertemu. Ojak juga menjadi jembatan yang menghubungkan pekerjaanmu dengan akun yang sesuai.

Dibuat oleh [lio](https://github.com/Liopark) dari [lapal](https://github.com/lapalai). [Lisensi MIT](../LICENSE) · [Pemberitahuan pihak ketiga](../THIRD_PARTY_NOTICES.md).
