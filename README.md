# clash-cli

一个简易 Rust 命令行代理工具。它不重写代理内核，而是编排
[mihomo](https://github.com/MetaCubeX/mihomo)：负责订阅更新、生成运行时配置、测试节点延迟，并通过
mihomo 的 external-controller 自动切换到可用节点。

## 功能

- 支持 JMS/机场提供的 Clash/Mihomo YAML 订阅更新
- 支持 base64 订阅 URI 列表里的 `ss://` 和 `vmess://` 节点
- 自动合并多个订阅的 `proxies`
- 支持 include/exclude 正则过滤节点
- 自动生成 `PROXY` 选择组和 `AUTO` url-test 组
- 自动 ping/延迟测试节点并切换到最快可用节点
- 当当前订阅节点全部不通时，自动重新更新订阅、reload 配置并再次切换
- 支持生成 TUN 虚拟网卡配置
- 支持配置自定义规则和 `rule-providers`

## 安装/构建

```bash
cd /Volumes/W_W/claude-workspace/clash-cli
cargo build --release
```

需要本机已有 `mihomo` 可执行文件，并确保它在 `PATH` 中，或在初始化时指定路径。

## 快速开始

最简单的首次启动：

```bash
/Volumes/W_W/claude-workspace/clash-cli/target/release/clash-cli \
  "https://your-jms-subscription.example/clash.yaml"
```

之后再启动只需要下面这一行。它会优先使用上次订阅生成的本地节点配置，不会每次启动都拉订阅：

```bash
/Volumes/W_W/claude-workspace/clash-cli/target/release/clash-cli
```

如果 `mihomo` 不在 `PATH` 中，首次启动时顺手指定一下：

```bash
/Volumes/W_W/claude-workspace/clash-cli/target/release/clash-cli \
  --mihomo-bin /usr/local/bin/mihomo \
  "https://your-jms-subscription.example/clash.yaml"
```

开发模式也可以这样跑：

```bash
cargo run -- "https://your-jms-subscription.example/clash.yaml"
```

如果订阅服务提供多个格式，优先使用 Clash/Mihomo YAML 格式；如果只有普通 base64 订阅，本工具也会尝试转换其中的 `ss://` 和 `vmess://` 节点。

## 常用命令

```bash
# 首次写入/替换订阅并直接启动
clash-cli "https://your-jms-subscription.example/clash.yaml"

# 后续直接启动
clash-cli

# 启用 TUN 并启动
sudo clash-cli --tun

# 替换订阅并启动
clash-cli --subscribe "https://your-jms-subscription.example/clash.yaml"

# 查看配置、缓存和运行时配置路径
clash-cli paths

# 拉取订阅并生成 mihomo runtime.yaml
clash-cli update

# 启动 mihomo，并周期性检查/自动切换
clash-cli run

# 如果你已经单独启动了 mihomo，只运行健康检查循环
clash-cli --no-core

# 单次测试并切换到最快可用节点
clash-cli switch

# 单次健康检查：不通则更新订阅并切换
clash-cli check

# 诊断代理是否真的生效
clash-cli doctor

# 诊断指定地址
clash-cli doctor "https://example.com"
```

也可以指定配置文件：

```bash
clash-cli --config ./config.yaml
```

## 配置示例

`init` 会生成平台默认配置文件。macOS 上通常位于：

```text
~/Library/Application Support/io.github.clash-cli.clash-cli/config.yaml
```

示例：

```yaml
subscriptions:
  - name: jms
    url: https://your-jms-subscription.example/clash.yaml
    user-agent: clash-cli/0.1
    include:
      - "香港|日本|新加坡|美国"
    exclude:
      - "官网|剩余|过期|到期|倍率"

mihomo:
  bin: /usr/local/bin/mihomo
  mixed-port: 7890
  allow-lan: false
  mode: rule
  log-level: info

controller:
  host: 127.0.0.1
  port: 9090
  secret: ""

proxy:
  selector: PROXY
  auto-group: AUTO
  test-url: http://cp.cloudflare.com/generate_204
  timeout-ms: 5000
  interval-secs: 300
  health-check-secs: 300

tun:
  enable: false
  stack: system
  auto-route: true
  auto-detect-interface: true
  dns-hijack:
    - any:53

dns:
  enable: true
  enhanced-mode: fake-ip
  fake-ip-range: 198.18.0.1/16
  nameserver:
    - 223.5.5.5
    - 119.29.29.29
  fallback:
    - https://1.1.1.1/dns-query
    - https://dns.google/dns-query

rules:
  - DOMAIN-SUFFIX,google.com,PROXY
  - DOMAIN-SUFFIX,github.com,PROXY
  - GEOIP,CN,DIRECT
  - MATCH,PROXY

rule-providers: {}
```

## TUN 注意事项

开启 `tun.enable: true` 后，mihomo 通常需要管理员权限或相应系统授权：

```bash
sudo /Volumes/W_W/claude-workspace/clash-cli/target/release/clash-cli --tun
```

TUN 的实际创建、路由和 DNS 劫持都由 mihomo 执行，本工具只负责生成对应配置。

## 自动化验证

```bash
cd /Volumes/W_W/claude-workspace/clash-cli
scripts/smoke-test.sh
```

这个脚本会启动本地临时订阅服务，验证构建、帮助信息、首次订阅保存、订阅更新生成 runtime、TUN 快捷开关和 `init` 子命令。

实际网络诊断可以用：

```bash
/Volumes/W_W/claude-workspace/clash-cli/target/release/clash-cli doctor
/Volumes/W_W/claude-workspace/clash-cli/target/release/clash-cli doctor "https://example.com"
```

它会检查配置、runtime、mihomo 控制端口、混合代理端口、直连访问、代理访问、TUN 线索和系统代理线索。

## 运行机制

1. `update` 下载订阅，读取订阅里的 `proxies`。
2. 根据 include/exclude 过滤节点。
3. 生成 runtime 配置，包含 `PROXY`、`AUTO`、TUN、DNS、规则。
4. `switch` 调用 mihomo API 测试 `PROXY` 组内节点延迟。
5. 选出最快且未超时的节点，并 PUT 到 `/proxies/PROXY`。
6. `run` 默认使用上次生成的本地 runtime 配置；首次传入订阅或本地 runtime 不存在时才拉订阅。
7. `check/run` 如果没有可用节点，会先更新订阅，再请求 mihomo reload 配置，然后重新测试并切换。
