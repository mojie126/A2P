# agent2api-providers 3.5.0-beta.1 资产草稿（fork）

基于官方 3.5.0-beta.1（`c5f3305`）；以下为 fork 增量改动，草稿正文由此文件自动生成，同版本的多次构建在本文件整合或追加。

## 修复

- 修复：小浣熊对话请求头对齐官方桌面客户端 1.1.9（Issue #542）：SDK 指纹与平台标识改声明值（Windows / x64 / `desktop-windows-x64`，不再探测宿主），客户端标识改发小写 `x-client-*` 且版本升至 v1.1.9，会话标识改为官方 `sess-0-<8hex>` 派生（同账号同会话稳定、官方形态透传），轮次 ID 每请求生成 UUID v4，补 `X-RACCOON-Title` 与 `X-RACCOON-Call-Kind`，头顺序逐字对齐官方，组织码仅组织账号发送
- 修复：ZCode 活动目录为空时仍回读账号名下已领取套餐，已领取但目录暂缺的计划不再显示为空；名单内已有计划继续以活动目录的显示信息为准

## 优化

- 优化：ZCode 客户端版本默认值对齐真实客户端 3.17.2（官方 changelog 最新版本，复测证实空套餐列表与版本号无关）
- 优化：果办新增只读客户端状态模拟：读取订阅、奖励、试用资源与续费状态，只返回白名单字段，不提交订单或修改续费，并明确不能模拟客户端本地数据库、TLS 指纹与 CloudWAF 挑战
- 优化：上游死代码清理：`workbuddy/billing/tools.rs` 与 `astudio/accounts.rs` 中无调用方函数移除，时区语义实测结论并入 `parse_date_text` 文档，编译告警清零

## 其它

- 更新：dev-build 的官方 action 升级 v7（checkout、setup-node、upload-artifact、download-artifact），消除 Node.js 20 弃用告警
- 更新：补记本轮果办客户端状态模拟、ZCode 已领取套餐回显的验证结果与上游排查结论
- 版本：3.5.0-beta.1 → 3.5.0-beta.1
