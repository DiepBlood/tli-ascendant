# Ascendant

《火炬之光：無限》（Torchlight: Infinite）的 Windows 輔助視窗，支援兩種玩法：

- **塔羅斯（Tartarus）**：讀取監獄盤面，建議下一步要去哪一格、吃掉或替換哪個囚犯、選哪個刑具或典獄長寶藏獎勵，以及要不要重新隨機。
- **渴血手術（Vorax）**：讀取藥劑卡、水箱與手術用具，建議這一手選哪張卡、加到哪個水箱，並顯示每個選項達到 840k 的機率與平均分數；終局戰鬥時顯示波次、活性與加成。

> 非官方工具，與《火炬之光：無限》的開發商、發行商無關，也未經其認可。
> Ascendant 只讀取遊戲視窗的畫面和遊戲自己寫出的日誌：不碰遊戲的記憶體、檔案或網路，也不會替你點擊或打字。

English summary at the [end of this page](#english).

## 支援的語言

設定頁的「Language / 語言」同時決定 Ascendant 視窗的文字，以及讀取遊戲畫面時用的語言，所以要設成**和遊戲客戶端相同**（Windows 也要裝同一種 OCR 語言，見[系統需求](#系統需求)）。目前只支援繁中和英文兩種客戶端；簡中、俄文等其他客戶端都不支援。

| 功能 | 繁中客戶端 | 英文客戶端 |
|---|---|---|
| 塔羅斯：讀盤面與建議 | 支援 | 支援，測試較少 |
| 渴血手術：讀卡片、水箱與建議（含終局戰鬥） | 支援 | 不支援：視窗頂端會顯示提示，塔羅斯照常 |
| 進度頁：累積次數、可玩局數 | 支援 | 應可使用（讀遊戲日誌裡不分語言的數值），尚未實測 |
| 本局頁：運氣／決策分析 | 支援 | 支援 |
| 匯出回報資料 | 支援 | 支援 |

- 本局頁的分析要用當局遊玩時的語言設定來看；語言不同時會提示先到設定頁切換。
- 作者平常都用**繁中客戶端**測試，所以繁中客戶端的畫面辨識與分析最完整，也最推薦。遇到讀錯請回報（方法見下方[回報問題](#回報問題)）。

## 下載前請先看

- 只從這個 repo 的 [Releases](../../releases) 頁面下載。其他網站、論壇、私訊、雲端硬碟傳來的「Ascendant」一律不要用。
- **已經裝有 Ascendant**：把下載的 Release 檔案放在同一個資料夾（至少要有 `Ascendant.exe`、`SHA256SUMS.txt`、`SHA256SUMS.txt.sig`），在已安裝的 Ascendant 設定頁「更新」群組按「**驗證下載的檔案…**」選擇那個資料夾，顯示「驗證通過」才執行。它用程式內建的發行公鑰檢查簽章與 `Ascendant.exe` 的雜湊；簽章清單列出的其他檔案（原始碼 zip、`RELEASE-NOTES.txt`）有放在資料夾裡的，也逐一比對雜湊，不符就不通過。驗證通過但出現警告時，要先處理再執行：「資料夾裡另有…」就把 `Ascendant.exe` 移到只放它的資料夾；「比目前執行中的版本還舊」就改下載最新版。只用 `Get-FileHash` 和沒有驗證簽章的 `SHA256SUMS.txt` 比對，證明不了檔案是作者發行的。
- **第一次安裝**（還沒有 Ascendant 可以用來驗證）：能信任的只有來源（本 repo 的 Releases）和下方公布的發行公鑰指紋。執行後到設定頁「關於」核對指紋，不同就立刻刪除。指紋相同不代表檔案一定沒問題（惡意程式也能顯示任何文字），所以一定要從 Releases 下載。
- 發行公鑰指紋：**924d 51f6 7734 fbb4 dad1 1091 77ee 6bd5**
- 第一次執行時，Windows SmartScreen 可能顯示「Windows 已保護您的電腦」。這是因為 Ascendant 沒有程式碼簽章（簽章憑證要付費）。確認檔案來自本 repo 的 Releases（已有 Ascendant 時，並用「驗證下載的檔案…」驗證通過）之後，才按「其他資訊」→「仍要執行」。
- 更多安全相關說明見 [SECURITY.md](SECURITY.md)。

## 系統需求

- Windows 10 或 11。
- 遊戲畫面比例 16:9 或更寬（1920×1080、2560×1080 等）；全螢幕、無邊框、視窗模式都可以。比 16:9 窄的畫面不會讀取。
- Windows 的 OCR 語言，要和遊戲客戶端的語言相同：
  - 繁中客戶端：`zh-TW`
  - 英文客戶端：`en-US`

  缺少時，盤面頁會提示。最簡單的做法是按程式裡的「**安裝繁中 OCR**」或「**安裝英文 OCR**」按鈕：會跳出 Windows 的系統管理員確認視窗，同意後自動安裝（下載可能要幾分鐘），完成後請重新啟動 Windows，再重新開啟 Ascendant。

  按鈕不能用時，手動安裝也可以：用系統管理員身分開啟 PowerShell，執行對應的一行，完成後重新啟動 Windows，再開啟 Ascendant：

  ```powershell
  Add-WindowsCapability -Online -Name "Language.OCR~~~zh-TW~0.0.1.0"
  Add-WindowsCapability -Online -Name "Language.OCR~~~en-US~0.0.1.0"
  ```

- 渴血的建議只支援**繁中客戶端**；塔羅斯兩種語言都支援。各功能的語言支援見[支援的語言](#支援的語言)。

## 安裝與使用

1. 執行 `Ascendant.exe`。第一次開啟時會詢問要不要安裝到目前的 Windows 使用者：會建立開始功能表、桌面捷徑，以及「應用程式與功能」裡的項目，不需要系統管理員權限。選「暫時不要」就直接從原地執行，設定頁裡會留一個「安裝」按鈕。
2. 進遊戲玩。視窗會固定在遊戲上方；移動過一次之後，下次會在同一個位置打開。
3. 視窗會依遊戲自動切換塔羅斯或渴血的盤面。

常用功能：

- **自動讀取**（預設開啟）：畫面一有變化就讀取。F8 開關自動讀取、F9 讀一次。遊戲在前景時會吃掉按鍵，所以這兩個鍵要在遊戲不是焦點時才有作用；快捷鍵可以在設定頁更改。
- **點擊穿透**：F7 開關。開啟後，點在視窗的空白處會穿到遊戲，按鈕、格子等元件仍然可以點。
- **版面**：自動／完整／緊湊。「自動」會在視窗較小時切到緊湊版。
- **字體大小**：標準／大／特大。
- **標示要點擊的位置**（預設關閉）：在遊戲畫面上用金色框標出建議要點的格子或按鈕。
- **不透明度**：最低 50%。
- **進度頁**：顯示渴血與塔羅斯的累積次數與可玩局數（從遊戲日誌讀取）。
- **本局頁**：一局結束後，分析這局的結果主要是運氣還是決策，並列出損失最大的一步。
- 讀錯的格子可以直接點來修正。

設定存在 `%APPDATA%\Ascendant\settings.json`，「還原預設值」按鈕可以回到預設設定。
上傳紀錄在這個版本是關閉的。

## 更新

- 設定頁的「啟動時檢查更新」預設開啟：啟動時和 Ascendant 開著的期間每小時，在背景向 GitHub 查詢最新版，也可以按「立即檢查」。檢查只會連到 GitHub，不會傳送任何紀錄；之後的檢查會帶上次的 ETag，最新版沒變時 GitHub 只回「沒有變更」。連不上或 GitHub 要求稍候時，就等一小時後的下一次檢查（GitHub 要求等更久就照它的時間），不會連續重試。開著時查到的新版只在左側設定圖示加一個小紅點，不會跳出提示、畫面也不會移動：頂端提示列不變，你正停在設定頁時，「有新版」區塊和檢查結果要等你離開設定頁後才更新。
- 更新完成、重新開啟後，頂端會顯示「已更新到 X.Y.Z」與「更新內容」，按 × 或玩完一局後就不再顯示；設定頁「關於」的版本旁邊也有「更新內容」可以隨時再看。按下會在 Ascendant 視窗裡依版本由新到舊列出每一版的更新內容（只顯示目前介面語言的那一段）：目前這一版展開，舊版本折疊，點版本號就能展開。各版的內容編在程式裡（和簽章的 `RELEASE-NOTES.txt` 相同），新安裝的人也看得到完整歷史；不需連網，也不會開啟瀏覽器。有新版時，提示列的「更新內容」會把尚未安裝的新版放在最上面並標示「尚未安裝」。
- 檢查時就會先驗證新版：`SHA256SUMS.txt` 的 Ed25519 簽章（`SHA256SUMS.txt.sig`）必須由作者電腦上的發行私鑰簽署（程式內建對應的公鑰）、簽署的清單必須是這個版本的、GitHub 提供的 `Ascendant.exe` 雜湊必須和清單一致、版本必須比目前新（拒絕降版），而且只連 GitHub 的主機。**全部通過才會顯示「有新版 X.Y.Z」與版本說明**（版本說明取自受簽章保護的 `RELEASE-NOTES.txt`，不是 GitHub Release 頁面上的文字）；沒通過就不會通知，設定頁只寫「此版本未通過驗證，請勿安裝」。連不上網路時只會寫「暫時無法檢查，稍後再試」。
- **只有你按「更新」才會下載**，不會在背景自動替換。下載後會再驗證一次，`Ascendant.exe` 的 SHA-256 也必須和簽署的清單相符，全部通過才替換並重新啟動；下載失敗或任何一項驗證不符，都會保留目前的版本（萬一是替換檔案本身失敗，Ascendant 會說明該怎麼做：把 `Ascendant.old.exe` 改回原名，或重新開啟 Ascendant）。
- 因此即使 GitHub 帳號被盜，沒有發行私鑰的人也推不出能安裝的更新。Ascendant 永遠不需要你手動下載檔案來替換。
- 關於頁顯示內建發行公鑰的指紋，應該和上方[下載前請先看](#下載前請先看)公布的相同；用已安裝的 Ascendant 核對下載檔案的方法見 [SECURITY.md](SECURITY.md#如何確認下載來源正確)。

## 紀錄

每一局在 `%APPDATA%\Ascendant\runs`（塔羅斯）或 `vorax`（渴血）底下有一個資料夾，裡面有：

- `log.jsonl`：每次讀取、建議與修正的紀錄。
- `frame-NNNN.jpg`：讀取時的遊戲畫面。

塔羅斯與渴血各只保留最近 10 局完成的對局（文字紀錄與截圖都完整保留），更舊的資料夾會整個自動刪除；完成的對局不滿 10 局時不會刪除任何東西。設定頁的「**保留全部紀錄**」（預設關閉）開啟後，所有紀錄與截圖都會保留、不會自動刪除，請自行注意磁碟空間。

要回報讀錯或建議不合理時，請用下方「回報問題」的方法，不需要自己去壓縮資料夾。

從舊版（`warden.exe`、`%APPDATA%\warden`）升級時，第一次開啟會自動沿用舊的設定，捷徑也會改指向 `Ascendant.exe`；舊資料夾會保留。

## 回報問題

遇到讀錯格子、建議不合理或程式異常時：

1. 在 Ascendant 的**本局頁**按「**匯出回報資料**」，選要回報的那一局，按「**準備匯出**」。
2. 畫面會先列出 zip 裡的內容給你看，這時還沒有存任何檔案；確認後按「**存檔並開啟資料夾**」才會寫入。zip 存在 `Ascendant.exe` 所在的資料夾（該資料夾不能寫入時改存到 `%APPDATA%\Ascendant`），檔案大小最多 20 MB。
3. 到本 repo 的 [Issues](../../issues/new/choose) 開新問題，填寫表單，並把這個 zip 拖進附件欄。

這個 zip 只含文字紀錄（`log.jsonl`、`report.txt`）與遊戲截圖：
- 塔羅斯的監獄盤面、渴血的選卡與結算畫面是**整張遊戲畫面**（縮小到 1280 寬）。塔羅斯的整張畫面會先把右下角（UID）與左下角（聊天、伺服器與識別碼）塗黑；渴血的選卡與結算畫面是全螢幕介面，畫面上沒有 UID 和聊天，不另外塗黑。其他畫面只裁出盤面，渴血終局戰鬥只有裁切過的面板。
- 文字紀錄會去掉遊戲視窗標題、本機路徑、Windows 使用者名稱與 15 位以上的長數字（玩家 ID 的格式）。

整張畫面仍可能拍到角色名稱等你不想公開的內容，請在按「存檔並開啟資料夾」之前先看過列出的內容。請只附這個 zip（或單張截圖），不要附 exe 或其他壓縮檔。

## 自行建置

需要 Rust（edition 2024），以及 Visual Studio Build Tools 附的 Windows SDK `rc.exe`（用來把圖示嵌進 exe）。

```powershell
cargo build --release --bin Ascendant       # target\release\Ascendant.exe
cargo build --release --bin ascendant-cli   # 命令列工具
```

## 授權與出處

- Ascendant 改自 [arcasti/tli-warden](https://github.com/arcasti/tli-warden)，依 **GPL-3.0** 授權釋出，全文見 [LICENSE](LICENSE)。
- 渴血求解器改自 vorax-solver-rs（**MIT**，作者 DinoRaos、tidewarden、Moebing），授權全文見 [crates/vorax/LICENSE](crates/vorax/LICENSE)。
- 遊戲名稱、圖示與畫面的權利屬於其各自的權利人。

---

## English

Ascendant is a Windows companion app for *Torchlight: Infinite*, modified from [arcasti/tli-warden](https://github.com/arcasti/tli-warden). It reads the game window and recommends moves for two modes:

- **Tartarus**: which destination to take, which Inmate to consume or replace, which Torture Instrument or Warden's Treasure reward to pick, and when to re-roll.
- **Vorax**: which card to pick and which tank to add it to, with each option's chance to reach 840k and its average score, plus the endgame battle's wave, activity and bonus. Vorax advice needs the Traditional Chinese game client.

Unofficial and not affiliated with the game's developers or publishers. It only reads pixels from the game window and the game's own log; it never touches the game's memory, files or network traffic, and never clicks or types for you.

**Client language**: the "Language / 語言" setting sets both the window's text and the language Ascendant reads the game in, so set it to the game client's language. Only the Traditional Chinese (`zh-TW`) and English clients are supported; Simplified Chinese, Russian and other clients are not.

| Feature | Traditional Chinese client | English client |
|---|---|---|
| Tartarus: board reading and advice | yes | yes, less tested |
| Vorax: cards, tanks and advice (endgame battle included) | yes | no: a notice shows at the top, Tartarus goes on as usual |
| Progress page: counts and games left | yes | should work (it reads language-neutral values from the game log), not tested yet |
| Run page: luck vs decisions | yes | yes |
| Export report data | yes | yes |

A run is reviewed in the language it was played in; when the setting differs, the Run page asks you to switch it first. The Traditional Chinese client is the most tested and is recommended.

**Requirements**: Windows 10 or 11; the game at 16:9 or wider; the Windows OCR language matching the game client (`en-US` or `zh-TW`). The app has "Install Traditional Chinese OCR" and "Install English OCR" buttons (they show a Windows administrator prompt; restart Windows afterwards, then reopen Ascendant); the PowerShell commands above are the manual fallback.

**Safe download**: only download from this repo's [Releases](../../releases) page. If you already have Ascendant, put the release files you downloaded (at least `Ascendant.exe`, `SHA256SUMS.txt` and `SHA256SUMS.txt.sig`) in one folder and check them with "Verify downloaded files..." in the settings page's "Updates" group, which checks the signature, `Ascendant.exe` and every other signed file that is in the folder: only "Verified" means the author released them, and a warning under it ("the folder also holds..." or "an older version") must be dealt with before running: move `Ascendant.exe` to a folder of its own, or download the latest version instead (comparing `Get-FileHash` with an unverified `SHA256SUMS.txt` proves nothing). On a first install nothing can check the signature yet, so trust rests on the source and on the release key fingerprint published on the "發行公鑰指紋" line above: compare it with the one in the About section and delete the program if it differs (a match alone does not prove the file genuine). The exe is not code-signed, so SmartScreen may warn: choose "More info" then "Run anyway" only once you know the file came from this repo's Releases. See [SECURITY.md](SECURITY.md).

**Reporting a problem**: on the Run tab press "Export report data", pick the run and press "Prepare". Its contents are listed first and nothing is written until you press "Save and open folder"; the zip (at most 20 MB) is saved next to `Ascendant.exe`, or in `%APPDATA%\Ascendant` when that folder cannot be written. Drag it into the [issue form](../../issues/new/choose). It holds text logs (with no window titles, local paths, user names or 15-digit-plus numbers) and game screenshots: Tartarus prison boards and Vorax choice and result screens are whole frames shrunk to 1280 wide; the Tartarus ones have the bottom right (UID) and bottom left (chat, server and id line) blacked out, while the Vorax choice and result screens are full-screen menus that show no UID or chat; other screens are cut to the board, and the Vorax endgame battle only as its cropped panel. A whole frame can still show things such as character names, so look the list over before saving. Attach nothing else.

**Use**: run `Ascendant.exe`, optionally install it for the current user, then play. F7 toggles click-through, F8 auto read, F9 reads once (while the game is not focused). Settings live in `%APPDATA%\Ascendant\settings.json`; run logs in `%APPDATA%\Ascendant\runs` and `vorax`, where Tartarus and Vorax each keep only their newest 10 finished games (logs and screenshots) and older folders are deleted automatically, unless "Keep all run logs" (off by default) is turned on, which keeps every log and screenshot (watch your disk space). Log sharing is off in this build.

**Updates**: "Check for updates at start" (on by default) asks GitHub for the latest release at start and then every hour while Ascendant is open, or when you press "Check now"; it only talks to GitHub and sends no logs. Later checks send the last answer's ETag, so an unchanged release costs GitHub only a "not modified"; when GitHub cannot be reached or asks to slow down, Ascendant waits for the next hourly turn instead of retrying. A new version found while Ascendant is open only puts a small red dot on the Settings button: nothing pops up and nothing moves (the top line stays as it is, and while you are on the settings page its "New version" group and check result change only once you leave it). After an update, the top line says "Updated to X.Y.Z" with "What's new" until you press × or play a game, and "What's new" next to the version in About shows them again any time inside the Ascendant window: every version's notes in the window's language, newest first, the current one open and older ones folded. The notes are built into the program (the same text as each signed `RELEASE-NOTES.txt`), so a fresh install sees the whole history too, without the network and without opening a browser; when a new version is found, its notes come first, marked "not installed yet". A newer version is verified during the check itself: the Ed25519 signature of `SHA256SUMS.txt` (`SHA256SUMS.txt.sig`) must verify against the public key built into Ascendant, the signed list must be for that version, GitHub's digest of `Ascendant.exe` must match it, the version must be newer (no downgrades), and every connection must go to GitHub's hosts. Only then is "New version X.Y.Z" shown with its release notes, taken from the signed `RELEASE-NOTES.txt`, never from the GitHub release page; otherwise there is no notice and the settings page says "This version failed verification. Do not install it." Without a network it only says to try again later. The update is downloaded only when you press "Update", verified again (including the exe's SHA-256) and only then swapped in; if the download or any check fails, the current version is kept (should replacing the file itself fail, Ascendant says what to do: rename `Ascendant.old.exe` back, or start Ascendant again). A stolen GitHub account alone cannot ship an update that installs, and Ascendant never needs you to download and replace files by hand. The About section shows the built-in key's fingerprint, which should match the published one; see [SECURITY.md](SECURITY.md) for checking downloaded files.

**Build**: Rust (edition 2024) and the Windows SDK's `rc.exe`; `cargo build --release --bin Ascendant`.

**License**: GPL-3.0, see [LICENSE](LICENSE). The Vorax solver is based on vorax-solver-rs (MIT; DinoRaos, tidewarden, Moebing), see [crates/vorax/LICENSE](crates/vorax/LICENSE).
