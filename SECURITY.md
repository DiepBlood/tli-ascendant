# 安全性政策（Security Policy）

## 如何確認下載來源正確

- **官方只在本 repo 的 [Releases](../../releases) 頁面發佈 Ascendant。** 其他網站、論壇、社群貼文、雲端硬碟連結傳來的檔案都不是官方版本。
- **我們不會私訊（或以任何方式主動）向你索取檔案、帳號或密碼。** Ascendant 不需要你的遊戲帳號、密碼或任何登入資訊。有人以「作者」名義要這些，一律是詐騙。
- **已經裝有 Ascendant：用它驗證下載的檔案。** 把下載的 Release 檔案放在同一個資料夾（至少要有 `Ascendant.exe`、`SHA256SUMS.txt`、`SHA256SUMS.txt.sig`；`Ascendant-<版本>-source.zip`、`RELEASE-NOTES.txt` 可有可無），在已安裝的 Ascendant 設定頁「更新」群組按「**驗證下載的檔案…**」，選擇那個資料夾。它用程式內建的發行公鑰檢查簽章，再檢查 `Ascendant.exe` 的 SHA-256；簽章清單列出的其他檔案，有放在資料夾裡的也逐一比對 SHA-256，任何一個不符就不通過（沒放的不檢查），通過時會列出另外比對過的檔案。顯示「驗證通過」才是作者發行的檔案，沒通過就不要執行。驗證通過但出現警告時，要先處理再執行：「資料夾裡另有…」表示旁邊還有其他 .dll 或 .exe，Windows 可能從 exe 所在的資料夾載入 DLL，請把 `Ascendant.exe` 移到只放它的資料夾；「比目前執行中的版本還舊」表示這是舊版，請改下載最新版。信任的依據是你已安裝的這份程式內建的公鑰，不是網頁上的任何文字；只拿 `Get-FileHash` 和沒有驗證簽章的 `SHA256SUMS.txt` 比對，證明不了檔案是作者發行的（兩者可能被一起換掉）。
- **第一次安裝（還沒有 Ascendant 可以用來驗證）**：這時沒有任何既有程式可以驗證簽章，能信任的只有兩件事：
  1. **來源**：只從本 repo 的 [Releases](../../releases) 頁面下載。
  2. **公布的指紋**：發行公鑰的指紋公布在本檔與 README（見下方），作者不會在其他地方用別的指紋。執行後到設定頁「關於」核對顯示的指紋；**不同就立刻刪除，不要再用**。注意：指紋相同不能證明檔案沒問題（惡意程式也能顯示任何文字），它只能抓出內建了別的金鑰的版本；第一次安裝的安全性取決於來源。
  安裝之後，所有更新與「驗證下載的檔案…」都用這份程式內建的公鑰驗證。
- 程式沒有程式碼簽章，Windows SmartScreen 可能警告。確認檔案來自本 repo 的 Releases（已有 Ascendant 時，並用「驗證下載的檔案…」驗證通過）之後，才按「其他資訊」→「仍要執行」。

### 發行公鑰指紋

發行公鑰指紋：**924d 51f6 7734 fbb4 dad1 1091 77ee 6bd5**（SHA-256 前 32 個十六進位字元，每 4 個一組，共 8 組；2026-11-06 產生）

## 更新與簽章驗證

- 每個 Release 附有 `SHA256SUMS.txt` 和它的 Ed25519 簽章 `SHA256SUMS.txt.sig`。`SHA256SUMS.txt` 列出 `Ascendant.exe`、原始碼 zip 與版本說明 `RELEASE-NOTES.txt` 的 SHA-256，所以版本說明也受簽章保護。簽章用的私鑰只存在作者的電腦上（以密碼加密），不在 GitHub；Ascendant 內建對應的公鑰。
- 檢查更新時就會驗證簽章、版本與雜湊，**全部通過才會顯示「有新版」**，顯示的版本說明只取自已驗證的 `RELEASE-NOTES.txt`，不採用 GitHub Release 頁面上的文字；按「更新」後會再驗證一次（包括下載的 `Ascendant.exe` 的 SHA-256），通過才替換。任何一項不符就顯示「此版本未通過驗證，請勿安裝」並保留目前的版本。所以即使有人盜用 GitHub 帳號上傳檔案，也無法推出會被安裝的更新。
- **公鑰指紋**：設定頁「關於」顯示內建發行公鑰的指紋（SHA-256 前 32 個十六進位字元，即 128 位元，每 4 個一組），應該和上方「發行公鑰指紋」相同，也可以和其他人互相核對。
- **任何要你手動下載、替換檔案的公告都不可信**，即使看起來來自本 repo 或作者。Ascendant 的更新一律經由程式內的自動更新完成，作者永遠不會要求你手動下載檔案替換。
- **換金鑰**：日後如果要換金鑰，一律發行一個「過渡版」：用舊金鑰簽署、程式內含新公鑰，透過自動更新安裝；之後的版本再用新金鑰簽署。這個過程不需要你做任何事；新指紋會在本檔與 README 公布（舊指紋保留並註明已停用），過渡版安裝後「關於」顯示的會是新指紋。私鑰檔有密碼加密，並另外備份；若舊私鑰遺失或密碼遺忘而無法簽出過渡版，舊版就不會再收到更新，但作者仍不會要求你手動下載替換，見到這類要求請當成詐騙。
- 命令列工具 `ascendant-cli verify` 是開發者自己核對用的工具；從原始碼建置出來的程式，其內建公鑰也來自同一份原始碼，所以**不能**當成判斷真偽的依據。

## 作者發行時的安全守則

- **發行前不盲目 pull 開發 repo**：發行用的電腦只建置自己審閱過的變更。從遠端拉下來的任何 commit（尤其是動到 `src/app/update.rs` 的發行公鑰、`scripts/`、`build.rs`、`Cargo.toml`／`Cargo.lock`、`.cargo/` 的）都要先看過差異再發行；帳號被盜時，被改的公鑰或建置腳本就是從這裡混進正式版。
- **換金鑰時核對指紋**：過渡版由發行腳本先證明新私鑰和程式內嵌的新公鑰是同一對，再列出新舊兩個指紋，要人工輸入 `yes` 才用舊私鑰簽章。舊指紋必須和上一版公布的相同，新指紋必須和產生新金鑰時 `keygen` 印出的相同，並且先公布在本檔與 README。
- 發行腳本會拒絕本檔與 README 沒有公布內建公鑰指紋的版本。

## 回報安全問題

請使用 GitHub 的 **Private vulnerability reporting**（本 repo 的 Security 分頁 → Report a vulnerability）私密回報，**不要開公開 issue**，以免在修正前就公開細節。

## 回報一般問題時

只附程式「匯出這一局」產生的 zip（本局頁下方的卡片，匯出頂端選單選的那一局；先列出內容給你看，按「存檔並開啟資料夾」才寫入，存在 `Ascendant.exe` 所在的資料夾，不能寫入時改存到 `%APPDATA%\Ascendant`；附上前請先看過內容，整張畫面可能拍到角色名稱）或截圖。**不要附 exe 或其他壓縮檔**；我們也不會要求你這麼做。

---

## English

- **Official builds are published only on this repo's [Releases](../../releases) page.** Files from anywhere else are not official.
- **We will never message you to ask for files, accounts or passwords.** Ascendant needs no game account or login. Anyone asking is a scammer.
- **Check downloaded files with the Ascendant you already have**: put the release files you downloaded into one folder (at least `Ascendant.exe`, `SHA256SUMS.txt` and `SHA256SUMS.txt.sig`; `Ascendant-<version>-source.zip` and `RELEASE-NOTES.txt` are optional), then on the settings page, in "Updates", press "Verify downloaded files..." and pick that folder. It checks the signature with the public key built into your installed copy, then the SHA-256 of `Ascendant.exe`, and of every other file the signed list names that is in the folder (any mismatch fails; files not there are not checked), and names the other files it checked. Only "Verified" means the author released them, otherwise do not run them. A warning under "Verified" must be dealt with before running: "the folder also holds..." means other .dll or .exe files sit beside it, and Windows may load a DLL from the folder an exe starts in, so move `Ascendant.exe` to a folder of its own; "an older version" means it is not the latest, so download the latest version instead. Trust rests on that built-in key, not on any web page. Comparing `Get-FileHash` with an unverified `SHA256SUMS.txt` proves nothing, since both can be replaced together.
- **First install (no Ascendant yet to verify with)**: nothing on your computer can check the signature yet, so you can only trust the source, this repo's [Releases](../../releases) page, and the published fingerprint below. After starting it, compare the fingerprint in the About section with the published one; if it differs, delete the program at once. A match does not prove the file is genuine (a malicious program can display any text); it only catches a build carrying another key. From then on, every update and "Verify downloaded files..." is checked with the installed copy's key.
- The exe is not code-signed, so SmartScreen may warn. Choose "More info" then "Run anyway" only once you know the file came from this repo's Releases (and, if you already have Ascendant, it passed "Verify downloaded files...").
- **Release key fingerprint**: published on the "發行公鑰指紋" line near the top of this page (8 groups of 4 hex digits: the first 32 hex digits, 128 bits, of the key's SHA-256). The About section shows the built-in key's fingerprint in the same form.
- **Signed updates**: every release carries `SHA256SUMS.txt` and its Ed25519 signature `SHA256SUMS.txt.sig`, made with a private key that lives only on the author's computer, never on GitHub. `SHA256SUMS.txt` lists `Ascendant.exe`, the source zip and the release notes `RELEASE-NOTES.txt`, so the notes are signed too. The in-app update verifies that signature with the public key built into Ascendant, then checks that `Ascendant.exe` matches the signed SHA-256 and GitHub's digest, that the version is newer, and that only GitHub's hosts are contacted; anything else is refused and the current version is kept. A stolen GitHub account alone cannot ship an update that installs.
- **The check verifies before it tells you anything**: "New version" appears only once the signature, version and hashes check out, and the notes shown come only from the verified `RELEASE-NOTES.txt`, never from the GitHub release page; "Update" verifies everything again, the exe's SHA-256 included, before it replaces anything. Otherwise you see "This version failed verification. Do not install it." and the current version is kept.
- **Any announcement asking you to download and replace files by hand is not to be trusted**, whoever it seems to come from. Updates always arrive through the in-app update.
- **Key changes** always come as a transition release signed with the old key and carrying the new public key, installed by the in-app update: nothing for you to do. The new fingerprint is published here and in the README (the old one kept, marked retired). The private key is password-encrypted and backed up; if the old key or its password were ever lost so that no transition release can be signed, old versions simply stop receiving updates, and you will still never be asked to replace files by hand.
- `ascendant-cli verify` is a developer's tool: a copy built from the repo's source carries the key from that same source, so it is not a basis for trust.
- **The author's release rules**: never blindly pull the development repo before a release (review every incoming change, above all to the release key in `src/app/update.rs`, the scripts and the build files); on a key change, the release script proves the new private key matches the new built-in key, shows both fingerprints and signs only after `yes` is typed, and they must match the published ones.
- **Report security issues privately** through GitHub's Private vulnerability reporting (Security tab, "Report a vulnerability"). Do not open a public issue.
- **When reporting ordinary problems**, attach only the zip made by the app's "Export this run" card on the Run tab (it exports the run picked in the picker at the top), or a screenshot. Never attach an exe or any other archive.
