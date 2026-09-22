# 安全生产构建与烧录约束

## 构建

安全生产固件使用独立构建目录，不复用开发或诊断配置。签名私钥必须位于仓库外，并由调用环境提供：

```bash
export INKWASH_SECURE_BOOT_SIGNING_KEY=/secure/offline/note4-secure-boot-v2.pem
./scripts/build-secure.sh
```

生成物位于 `rust-firmware/target-secure/`。配置固定启用：

- ESP32-S3 Secure Boot V2，RSA-3072 签名
- AES-256 Flash Encryption，release 模式
- NVS Encryption，密钥由 Flash Encryption 保护
- 禁用 JTAG 和 ROM Basic Console
- DIO、80 MHz、16 MB Flash
- `rust-firmware/partitions.csv`

CI 使用一次性 RSA 密钥验证安全配置能够完整构建；一次性密钥生成的镜像不得用于量产。

## 私钥管理

- 量产私钥不得提交到 Git、CI secret、构建产物或设备备份。
- 签名应在受控离线环境或硬件安全模块中完成。
- 备份至少保留两份，分别加密保存并控制访问；私钥遗失后无法为已启用 Secure Boot 的设备发布新固件。
- 同一产品信任域内的密钥轮换策略必须在首次烧写前确定。

## 首次烧录

首次启用 Secure Boot、Flash Encryption 和调试接口禁用会写入不可逆 eFuse。只能在具备独立供电、可恢复工装和专用量产样机的环境执行。

每次烧录必须在写入前核验：

- 芯片为 ESP32-S3
- Flash 容量为 16 MB
- Flash 模式为 DIO、频率为 80 MHz
- 分区表来自当前构建的 `rust-firmware/partitions.csv`
- 设备身份属于本次量产批次的允许列表
- bootloader、partition table、OTA metadata 和应用来自同一次安全构建

当前授权开发设备 `20:6E:F1:B4:7D:E4` 不用于首次安全配置验证，不得在该设备上烧写安全 eFuse。

## 量产验收

量产工装必须自动检查并保存以下结果：

1. eFuse 摘要符合批准模板，Secure Boot V2 和 Flash Encryption 已启用。
2. 签名应用正常启动，NVS 写入和重启读取正常。
3. 未签名应用无法启动。
4. 离线读取 Flash 不包含 Wi-Fi 密码或 Bearer Token 明文。
5. OTA 更新只接受受信任密钥签名的镜像；失败更新回滚到上一槽。
6. JTAG 和 ROM Basic Console 不可用。
7. 设备 MAC、固件版本、镜像摘要和工装结果写入量产记录。
