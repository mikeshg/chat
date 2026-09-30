# 打包机器（Gitea runner）安装全流程

推 `v*` 标签后，[release.yml](../.gitea/workflows/release.yml) 要两台打包机器：

| 机器 | runner 标签 | 打什么 |
|---|---|---|
| Ubuntu（22.04 / 24.04，x86_64） | `linux` | 建 Release；deb + AppImage |
| Windows 10 / 11（x64） | `windows` | NSIS 安装包 + Android APK |

两台都用 host 模式：job 直接在机器上跑，工具链装一次就行，不需要 Docker。runner 程序现在叫 `gitea-runner`（原来的 act_runner），下面用 4.0.0。

预计耗时：Ubuntu 约 30 分钟，Windows 约 1–2 小时（主要花在 Android Studio 和 SDK 下载上）。

---

## 0. 先做：拿注册令牌、加 Secrets（任意一台电脑的浏览器）

1. 打开 `http://192.168.6.210:3000/xiefan/chat/settings/actions/runners`，点「创建新的 Runner」，复制注册令牌。两台机器可以共用这个令牌。
2. 打开 `http://192.168.6.210:3000/xiefan/chat/settings/actions/secrets`，添加两个 Secret：
   - `TAURI_SIGNING_PRIVATE_KEY`：开发机上 `~/.tauri/chat-updater.key` 的全部内容。
   - `ANDROID_DEBUG_KEYSTORE`：以前打 APK 那台电脑（开发机）上 debug keystore 的 base64。在那台电脑的 PowerShell 里执行下面这行，结果会复制到剪贴板：
     ```powershell
     [Convert]::ToBase64String([IO.File]::ReadAllBytes("$HOME\.android\debug.keystore")) | Set-Clipboard
     ```
3. 把 `~/.tauri/chat-updater.key` 和 `~/.android/debug.keystore` 备份到安全的地方（密码管理器或加密盘）。丢了私钥，就再也推不了在线更新，只能让大家手动重装。

---

## 1. Ubuntu runner

下面的命令都在 Ubuntu 上用有 sudo 权限的账号执行。

### 1.1 系统依赖、Node、pnpm

```bash
sudo apt update
sudo apt install -y git curl wget file python3 build-essential \
  libwebkit2gtk-4.1-dev libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev xdg-utils
curl -fsSL https://deb.nodesource.com/setup_22.x | sudo -E bash -
sudo apt install -y nodejs
sudo npm i -g pnpm@11
```

### 1.2 建 runner 专用账号，装 Rust

```bash
sudo useradd --system --home-dir /var/lib/gitea-runner --create-home --shell /bin/bash gitea-runner
sudo -u gitea-runner -H bash -c 'curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y'
```

### 1.3 装 gitea-runner 并注册

```bash
sudo curl -L -o /usr/local/bin/gitea-runner https://dl.gitea.com/gitea-runner/4.0.0/gitea-runner-4.0.0-linux-amd64
sudo chmod +x /usr/local/bin/gitea-runner
cd /var/lib/gitea-runner
sudo -u gitea-runner gitea-runner register --no-interactive \
  --instance http://192.168.6.210:3000 --token <注册令牌> \
  --name ubuntu-builder --labels linux:host
```

注册成功会在 `/var/lib/gitea-runner/` 下生成 `.runner` 文件。

### 1.4 注册成开机服务

```bash
sudo tee /etc/systemd/system/gitea-runner.service >/dev/null <<'EOF'
[Unit]
Description=Gitea Actions runner
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/gitea-runner daemon
WorkingDirectory=/var/lib/gitea-runner
User=gitea-runner
Group=gitea-runner
# systemd 不读 ~/.cargo/env，cargo 要显式加进 PATH
Environment=PATH=/var/lib/gitea-runner/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
# 需要代理才能访问外网时取消注释，并且把 Gitea 加进 no_proxy：
# Environment=http_proxy=http://代理:端口 https_proxy=http://代理:端口 no_proxy=192.168.6.210,localhost,127.0.0.1
Restart=on-failure
RestartSec=5s
TimeoutStopSec=3h

[Install]
WantedBy=multi-user.target
EOF
sudo systemctl daemon-reload
sudo systemctl enable --now gitea-runner
```

### 1.5 自检

```bash
sudo -u gitea-runner -H bash -lc 'cargo -V && node -v && pnpm -v && git --version && python3 -V'
systemctl status gitea-runner --no-pager
```

四个版本号都能打印出来，服务状态是 `active (running)`，就装好了。刷新第 0 步的 Runners 页面，能看到 `ubuntu-builder`，状态为空闲。

---

## 2. Windows runner（Windows 安装包 + Android APK）

用一个**固定的登录账号**来装和跑，下面叫它「打包账号」。所有工具和环境变量都装在这个账号下，runner 也以它的身份运行。命令在 PowerShell 里执行。

### 2.1 基础工具

```powershell
winget install -e --id Git.Git
winget install -e --id Microsoft.PowerShell
winget install -e --id OpenJS.NodeJS.LTS
winget install -e --id Rustlang.Rustup
winget install -e --id Microsoft.VisualStudio.2022.BuildTools --override "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
winget install -e --id Google.AndroidStudio
```

装完**关掉 PowerShell 再重新打开**（让 PATH 生效），然后执行：

```powershell
npm i -g pnpm@11
rustup default stable-msvc
rustup target add aarch64-linux-android armv7-linux-androideabi i686-linux-android x86_64-linux-android
```

### 2.2 Android SDK / NDK

1. 打开 Android Studio，按向导装好默认 SDK。
2. 打开 More Actions → SDK Manager → SDK Tools，勾选 **NDK (Side by side)** 和 **Android SDK Command-line Tools**，点 Apply。
3. 设置用户环境变量。路径是默认安装位置，NDK 的版本号按实际装的那个目录填：
   ```powershell
   [Environment]::SetEnvironmentVariable("JAVA_HOME", "C:\Program Files\Android\Android Studio\jbr", "User")
   [Environment]::SetEnvironmentVariable("ANDROID_HOME", "$env:LOCALAPPDATA\Android\Sdk", "User")
   $ndk = (Get-ChildItem "$env:LOCALAPPDATA\Android\Sdk\ndk" | Sort-Object Name | Select-Object -Last 1).FullName
   [Environment]::SetEnvironmentVariable("NDK_HOME", $ndk, "User")
   ```
4. 打开开发者模式：设置 → 系统 → 开发者选项 → 开发人员模式。`tauri android build` 需要建符号链接，不开会失败。

### 2.3 装 gitea-runner 并注册

```powershell
New-Item -ItemType Directory -Force C:\gitea-runner | Out-Null
cd C:\gitea-runner
curl.exe -L -o gitea-runner.exe https://dl.gitea.com/gitea-runner/4.0.0/gitea-runner-4.0.0-windows-amd64.exe
.\gitea-runner.exe register --no-interactive --instance http://192.168.6.210:3000 --token <注册令牌> --name win-builder --labels windows:host
```

### 2.4 开机自动运行（计划任务，以打包账号身份）

打开一个**管理员** PowerShell，执行下面的命令。它会提示输入打包账号的登录密码，这个密码只交给 Windows 保存，不会写进任何文件：

```powershell
schtasks /Create /TN "gitea-runner" /SC ONSTART /RU "$env:USERDOMAIN\$env:USERNAME" /RP * /RL HIGHEST /TR "cmd /c cd /d C:\gitea-runner && gitea-runner.exe daemon >> runner.log 2>&1"
schtasks /Run /TN "gitea-runner"
```

- 以后改了环境变量或新装了工具，要让 runner 读到新的环境，先执行 `schtasks /End /TN gitea-runner`，再执行 `schtasks /Run /TN gitea-runner`。
- 需要代理时，把代理设成打包账号的用户环境变量 `http_proxy`、`https_proxy`、`no_proxy`（`no_proxy` 里要有 `192.168.6.210`），然后按上一条重启任务。

### 2.5 自检

新开一个 PowerShell，执行：

```powershell
cargo -V; node -v; pnpm -v; git --version; pwsh -v; echo $env:JAVA_HOME $env:ANDROID_HOME $env:NDK_HOME
Get-Content C:\gitea-runner\runner.log -Tail 5
```

版本号和三个路径都能打印出来，日志里没有报错，Runners 页面能看到 `win-builder` 且状态为空闲，就装好了。

---

## 3. 服务端设置

新版服务端部署之后（`docker compose up -d --build`），用管理员账号打开后台管理 →「客户端在线更新」，填 `http://192.168.6.210:3000/xiefan/chat` 并保存。

---

## 4. 第一次发版（顺便验证整条链路）

在开发机上执行：

```bash
# 先把 app/src-tauri/tauri.conf.json 的 version 改成 0.2.0 并提交推送
git tag -a v0.2.0 -m "支持在线更新"
git push origin v0.2.0
```

1. 打开 `http://192.168.6.210:3000/xiefan/chat/actions`，等 release、windows、linux 三个 job 都变绿。第一次编译较慢，预计 20–40 分钟。
2. 打开 Releases 页面，`v0.2.0` 下应该有 7 个文件：`*_x64-setup.exe` 及其 `.sig`、`*_arm64.apk`、`*_amd64.deb` 及其 `.sig`、`*_amd64.AppImage` 及其 `.sig`。
3. 各台设备**手动安装一次** 0.2.0，因为旧版里没有更新功能。之后发 0.2.1，就能在「我」页的「应用更新」里直接看到并升级了。

## 出问题时先看这里

| 现象 | 原因 / 处理 |
|---|---|
| job 一直排队，不开始跑 | 没有 runner 带对应标签：检查 Runners 页面，确认 runner 在线，标签是 `linux` / `windows`。 |
| 「建 Release」或「上传」报 403 | 自动令牌没有写 Release 的权限：建一个有 `write:repository` 权限的个人令牌，存成 Secret `RELEASE_TOKEN`，再把 workflow 里的 `gitea.token` 换成 `secrets.RELEASE_TOKEN`。 |
| 下载 NSIS / linuxdeploy / Gradle 超时 | 打包机器访问不了 GitHub、Google 或 Maven：按上面的方法配代理。 |
| Android 报 symlink 或 `os error 1314` | 没开开发者模式（见 2.2 第 4 步）。 |
| 报 `A public key has been found, but no private key` | 没配 Secret `TAURI_SIGNING_PRIVATE_KEY`，或者内容没粘贴完整。 |
| 手机提示「应用未安装」或「签名冲突」 | 这个 APK 和手机上装着的版本签名不同：`ANDROID_DEBUG_KEYSTORE` 必须来自以前打包那台电脑。 |
| 装的是 deb，更新时要输入密码 | 正常现象：deb 更新要用 root 权限执行 `dpkg -i`。用 AppImage 装的不需要密码。 |
