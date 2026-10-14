# sd-xfyun

## 效果

https://github.com/user-attachments/assets/35a7ed2e-369e-4eb7-a7b5-6d2485c81433

把讯飞 AIkit 离线 XTTS 接成 speech-dispatcher 的输出模块：桌面上任何走 speech-dispatcher 的程序
（Orca、浏览器"朗读"、`spd-say`、自研客户端）都能用讯飞的离线声音说话，**合成不需要联网**。

包含：`sd_xfyun`（speech-dispatcher 输出模块）与 `xfyun-say`（命令行合成工具）。

- 流式合成：引擎每约 30ms 吐一块 PCM，边生成边播放，首字延迟低
- `STOP`/`PAUSE` 按句打断；默认把音频交给 speech-dispatcher 播放（705 协议），也可由模块自播放
- 发音人列表按本机 `xtts/` 资源扫描得出，不会把不存在的发音人下发给引擎

## 一、先申请自己的讯飞密钥与授权（必须）

**本项目不含任何可用密钥，必须自己申请。** 到讯飞控制台创建应用并申请「离线语音合成 （新版）」，拿到
appID / apiKey / apiSecret。

授权按**设备数**计量：首次使用会联网激活一次、占用一个装机量，之后授权缓存在
`~/.local/share/sd-xfyun/work/`（**别删**，删了要重新激活、又占一个装机量），激活后可完全离线使用。
## 二、依赖

- 讯飞 AIkit 离线合成 Linux SDK 压缩包解压到 `3rd_party/` 下
  （或用 `XFYUN_SDK_ROOT` 指定）
- Rust 1.75+；speech-dispatcher 0.10+
- 打包需 `cargo`、`patchelf`；模块自播放模式还需 `pw-play`/`paplay`/`aplay` 之一

## 三、构建（密钥在构建期写入二进制）

在源码树根目录建 `keys.toml`（**不入库**；fork 用户把它放在自己的私有仓库里，`cargo build` 直接读）：

```toml
XFYUN_APP_ID = "你的 appID"
XFYUN_API_KEY = "你的 apiKey"
XFYUN_API_SECRET = "你的 apiSecret"
```

```bash
cargo build --release     # 也可用环境变量 XFYUN_APP_ID / XFYUN_API_KEY / XFYUN_API_SECRET 临时覆盖
cargo test                # 47 个单元测试 + 2 个协议级端到端测试
```

密钥用 `obfstr` 在编译期混淆，明文不会出现在二进制的 `.rodata` 里。
但这并不是防破解手段。

## 四、打包

目前只有 PKGBUILD，Arch Linux 可直接用：

```bash
cd packaging && makepkg -f
sudo pacman -U sd-xfyun-*.pkg.tar.zst
```

模块 `/usr/lib/speech-dispatcher/speech-dispatcher-modules/sd_xfyun`、命令 `/usr/bin/xfyun-say`、
SDK 库 `/usr/lib/sd-xfyun/`、资源 `/usr/share/sd-xfyun/xtts/`、
配置 `/etc/speech-dispatcher/modules/xfyun-tts.conf`。

再把下面三行加进 `~/.config/speech-dispatcher/speechd.conf`（用户优先）或 `/etc/speech-dispatcher/speechd.conf`：

```
AddModule "xfyun" "sd_xfyun" "xfyun-tts.conf"
DefaultModule xfyun
LanguageDefaultModule "zh" "xfyun"
```

```bash
systemctl --user restart speech-dispatcher.socket   # 或 pkill speech-dispatcher
spd-say '你好，这是讯飞离线合成'
```
## 五、使用

```bash
spd-say '你好'                        # 默认模块已设为 xfyun
spd-say -O xfyun '指定模块朗读'
xfyun-say --list-voices               # 本机可用发音人（以 xtts/ 资源为准）
xfyun-say --text "你好" --out hello.wav
xfyun-say "直接给文本" --play
xfyun-say --voice xiaofeng --rate 60 --text "换发音人和语速" --play
```

## 六、配置

模块配置在 `/etc/speech-dispatcher/modules/xfyun-tts.conf`（`backup=`，可放心改）：

| 键 | 默认 | 说明 |
| --- | --- | --- |
| `XfyunSdkRoot` / `XfyunLibsDir` / `XfyunResDir` | 安装路径 | SDK 根目录 / 动态库目录 / 资源目录（其下要有 `xtts/`） |
| `XfyunWorkDir` | `$XDG_DATA_HOME/sd-xfyun/work` | 授权缓存与日志目录，**保持稳定** |
| `XfyunLicenseFile` | 无 | 离线激活文件/已有授权文件（只复制不移动） |
| `XfyunOfflineOnly` | `0` | `1` = 缺授权文件就报错，绝不联网激活 |
| `XfyunDefaultVoice` | `xiaoyan` | 默认发音人（本包只带 `xiaoyan`/`xiaofeng` 资源） |
| `XfyunSpeed` / `XfyunPitch` / `XfyunVolume` | `50` | 0–100 |
| `XfyunServerAudio` | `1` | `1` 交给 speech-dispatcher 播放；`0` 模块自播放 |
| `XfyunPlayer` | 自动探测 | 自播放时的播放器命令，如 `"pw-play"` |
| `XfyunMaxChunk` | `120` | 单次送入引擎的最大字数（越小首字越快、STOP 越及时） |
| `XfyunLogLevel` | `1` | `0`=error `1`=warn `2`=info `3`=debug `4`=trace |
| `XfyunReexec` | 开发 `1` / 打包 `0` | 见下 |

`XfyunReexec` 的来由：SDK 是用**裸文件名** `dlopen` 引擎库的，而引擎库没有 SONAME
（`ldconfig` 不会索引它），所以要么让模块启动时补 `LD_LIBRARY_PATH` 再 exec 一次（`1`，开发树走这条），
要么像 PKGBUILD 那样 `patchelf --set-rpath '$ORIGIN' libaikit.so`（它和引擎库同目录）后关掉（`0`）。

## 七、排查

| 现象 | 处理 |
| --- | --- |
| `18201 引擎动态库加载失败` | 引擎库没被找到：确认 `XfyunLibsDir` 下有 `*_aee.so`；开发构建保持 `XfyunReexec 1`，打包版应为 `0` 且 `libaikit.so` 已 patchelf |
| `18105 资源加载失败` | `XfyunResDir` / `XfyunWorkDir` 下没有 `xtts/` |
| `18501 参数范围溢出` | 请求的发音人在本机资源里不存在（日志会给可用列表并回退到默认发音人） |
| `18708 无可用授权` | 装机量已满，或该能力不支持当前平台/架构 |
| `18013 未找到当前设备` | 授权文件与本机设备指纹不匹配：联网激活一次，或申请包含本机的离线激活文件 |
| 没有声音 | `XfyunLogLevel 3` 看模块日志：服务端播放模式查 speech-dispatcher 的音频后端，自播放模式查播放器是否存在 |

## 八、已知限制

- 不支持 index mark（光标/索引事件），暂停要等当前句读完
- SSML 只做标签剥离（仅当文本以 `<speak` 开头时处理）
- 输出固定 16000Hz / 单声道 / 16bit PCM；仅 Linux x86_64

模块协议、705 音频打包、SDK 相关的坑都写在源码注释里。
