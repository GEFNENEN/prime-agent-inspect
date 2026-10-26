# prime-agent-inspect

prime-agent 会话的只读分析器。回答三个问题：会话里有什么、token 花在哪、每次压缩付了什么代价。

独立的 Cargo 项目，不依赖 prime-agent workspace，只依赖 `serde_json`。它自己解析会话 JSONL 而不链接 `pa-types`，这样上游改格式不会让分析器编译不过。

## 为什么不用内置 `/export`

`/export` 把整个会话 base64 塞进一个 HTML，靠浏览器渲染。单会话可以，但真实语料是 111 个会话、418 MB，其中一个 183 MB。浏览器装不下，也没有跨会话视角，更算不出 token 分解。

## 构建

```bash
cargo build --release
# 产物: target/release/prime-agent-inspect[.exe]
```

## 用法

```bash
prime-agent-inspect serve  <sessions-dir>            # 浏览器 UI（默认 127.0.0.1:7700）
prime-agent-inspect serve  <sessions-dir> --open     # 同上，并自动打开浏览器
prime-agent-inspect serve  <sessions-dir> --port N   # 换端口
prime-agent-inspect list   <sessions-dir>            # 所有会话概览
prime-agent-inspect show   <session.jsonl>           # 单个会话的 prompt 组成 + 压缩时间线
prime-agent-inspect check  <session.jsonl>           # 估算与实测的偏差
```

`sessions-dir` 即 `<agent_dir>/sessions`。

**推荐用 `serve`**。命令行适合脚本和快速核对，日常排查用浏览器更顺手：列表可点、曲线可看、刷新即重新快照。

### 内存占用

不保留正文，只记字符数与类型。p8 的 53,731 条目约 10 MB；全库 418 MB 在内存里是几十 MB 量级。服务器本身是单线程 + 一个 TCP 监听，几百 KB。

明显的开销在浏览器（100–300 MB），但那是你本来开着的窗口，不是这个工具新增的。做成原生 GUI（egui/winit）反而要打包 GPU 上下文、二进制大十几 MB、且只能单平台——所以本地 server + 浏览器是更划算的形态。

**只绑 127.0.0.1**，不接受外部连接：它读得到整个会话库的明文。

## token 数字从哪来

### `list`

```
session                                    size entries   comp      peak    newest  name
01a0a522-f0a1-756d-9792-ff4ac3781d66     170.3M   40288    214   1033844    152703  p6
01a0ae70-57d8-7110-9a1a-6e1b132036d8      15.0M    3359     11    241315    129308  p7
01a11fb8-d70b-73c7-bd47-f14868fab9ce     183.2M   53591    221   1033844     ...
```

`peak` 是这条分支上出现过的最高 prompt —— 判断「离上限多近」用这个。`newest` 是最后一次请求。

### `show`

「当前 prompt 组成」回答的是**下一轮会发出去什么**，而不是文件里有多少内容。这是排查上下文问题的关键区别：

```
  保留 assistant                    69092      33052   43.8% EST
  保留 toolResult                   48542      22996   30.5% EST
  摘要 summary                      30359      17409   23.1% EST
  kernel ipython_state             1564        708    0.9% EST
  TOTAL                          152008      75497
  (110 entries kept, 53481 dropped)
```

组装规则镜像 `pa-core` 的 `build_session_context`：取分支上最新的一次压缩，输出它的 summary，再从 `firstKeptEntryId` 起回放保留尾。两条容易漏的规则也照做了 —— 只保留最新的 `ipython_state`、只保留最新的 `harness_digest`（压缩自带的快照优先于外部 digest）。

「压缩时间线」每行是一次压缩：

```
  when                      before     after  retained  notice copies turns
  10-25 07:37:28.883        176302     78788     38459   15560      1    79
  10-26 04:29:56.395        193842     75005     33268     714      1    78
```

- `before` 压缩前的 prompt（`tokensBefore`，实测）
- `after` 压缩后第一次请求的 prompt（实测，含 cacheRead）
- `retained` 保留尾的估算 token
- `notice` `ipython_state` 占的 token —— 上面倒数第二行到最后一行的 15560 → 714，就是上游 `e2bcc6add` 给名字清单封顶 200 个/4KB 生效的时刻
- `copies` 组装后实际保留的副本数
- `turns` 距上一次压缩过了几轮，很小说明在连环压缩

### `check`

估算可信度自证：

```
217 compactions checked: median |error| 3.7%, worst 30.1%
fixed overhead 15865 tokens (measured from 217 point(s), not in the session file)
```

## Web UI

左侧是会话列表（按最后活动时间倒序），点击查看详情。顶部「刷新」重新扫描并重新快照；`Ctrl+R` 同效。

对话详情页是**概览 + 页签**结构。概览固定在页签之上，回答「这个会话有多大、离上限多近、烧了多久」；
其余内容一次只展开一页，避免六张卡片堆成三千像素的滚动条。

**概览** — 三块头条数字，然后一行事实。

- **当前 prompt**：重放出来的总量 = 正文 + 固定开销，下面一行把加法写明。
- **其中正文**：占整个 prompt 的百分比，配一条双色占比条（正文 / 固定开销，颜色与环形图一致）。
- **实测峰值**：provider 自己上报过的最大的 prompt、出现时间、最近一次的数值。
  峰值和「当前」差多少，是判断这个会话有没有顶到上限的唯一硬证据。

事实行：存在时长（并列出起止时刻）、压缩次数与平均轮数、条目数、文件大小与每条平均字节、
模型调用次数、缓存命中率、分支数、CJK 占比。工作目录单独一行，长路径换行不撑破网格。

**页签**（每个页签标题后带该页的关键计数，不点开也知道里面有什么）：

| 页签 | 内容 |
|---|---|
| 上下文组成 | 环形饼图 + 图例 + 保留内容的工具细分表 |
| 压缩历史 | Canvas 折线图 + 可滚动明细表，同一个「显示最近 N 次」控制两者 |
| 条目构成 | 整条分支按条目类型统计：数量、字符、字符占比、约 tokens |
| 重复注入 | 按 `customType` 统计的重复消息：份数、字符、占比、每次约多少 |
| 工具 | 每个工具的调用次数、结果字符、占全部结果、平均每次、失败次数 |
| 用量与模型 | 发送总量、缓存命中率、生成量、子代理开销，以及按模型的拆分 |
| 每日活动 | 逐日的条目数与 prompt/生成量，带条形比例 |
| 账本 | 子代理开销按来源拆分、自定义类型计数、git 提交列表 |

**上下文组成**：扇形按**整个 prompt**（正文 + 固定开销）切分，中心数字就是 `prompt.fullTokens`，
各块占比之和为 100%。悬停出提示框：tokens、占整个 prompt、占正文、字符数、CJK 字符数、条目数；
固定开销那块说明它是从多少个压缩点反推的。**点击扇形或图例行**打开正文窗口：按条目分页列出这一块的原文
（`/api/slice`，每块取前 600 字符，一次 12 条），滚动区倒序、最新在上。
正文按需从文件里 seek 读取，不进缓存——`Session` 只存字符数，187 MB 的会话不会因为开一次弹窗就整体重读。

**压缩历史**：红是压缩前、绿是压缩后、蓝色面积是保留尾。间隔轮数小于 10 的点标红，那是在连环压缩。
「显示最近 N 次」同时控制折线和明细表：默认 50，填 0 显示全部（230 次压缩那种会话一次画完曲线会糊成一坨）。
取值记在 `localStorage`（键 `prime-agent-inspect.recent`）。注意 `localStorage` 按 origin 隔离：
换端口或换浏览器就是另一份记忆。当前页签也存在 `localStorage`（键 `prime-agent-inspect.tab`），刷新后停在同一页。

**悬停任意一组**（图上的点，或明细表的任意一行）出提示框：时间、压缩前/压缩后（实测）、
保留尾、摘要、压掉多少与占比、保留条目数、重复通知、间隔轮数，以及只有这一步才有的
**估算 vs 实测误差**（明细表里没有这一列）。悬停时图上画一条虚线和环，表格里对应行高亮，
两边靠压缩 id 关联，不靠行号——「显示最近 N 次」改了两边一起重画。

表头每列的**小问号**解释这一列在数什么（悬停或点击）。写这块的原因是名字会骗人：
「保留尾」不是"压缩后剩下的全部"，它已经扣掉了摘要和 ipython_state 通知；
「压缩后」取的是下一次真实调用的 usage，而那一次调用本身带着新消息，所以偏大。
这类定义不写清楚，读者会把 `压缩后 − 保留尾` 的差额当成 bug。
「压掉」为负会显示成「反而增大」：压缩也会让 prompt 变大（短尾上做摘要，成本高于收益）。

刷新语义：列表和详情都**按请求重算**，按刷新看到的就是那一刻文件里的内容。服务端按 `(路径, 大小, 修改时间)` 缓存解析结果，文件没变直接返回，变了才重读一次。

### 性能

| 操作 | 冷 | 热 |
|---|---|---|
| `list`（112 会话 / 418 MB） | 1.4 s | 0.01 s |
| p8 详情（184 MB / 53,731 条目 / 223 次压缩） | 0.08 s | 0.03 s |
| 小会话 | 0.01 s | — |

早期版本 p8 详情要 19 秒：`overhead` 为求固定开销的中位数，会对每个压缩点重新组装一次整条分支，223 × 53,731 就是 O(n²)。改成从时间线已有数据推导后降到 0.08 s。

## token 数字从哪来

**实测优先**：`assistant` 消息里的 `usage` 是 provider 报的 `input`/`output`/`cacheRead`/`cacheWrite`。`prompt = input + cacheRead + cacheWrite` —— 只取 `input` 会漏掉缓存前缀，那是长会话 prompt 的绝大部分。

**估算兜底**：`custom_message`、`toolResult` 没有 usage，靠字符数估算。`chars/4` 在这个负载上错得离谱，实测是 2.21（纯 ASCII）到 1.30（全中文）字符/token。系数按 CJK 占比分桶，取自 19,116 组「字符数 × provider 实测增量」的中位数：

| CJK 占比 | 字符/token |
|---|---|
| 0.0 | 2.21 |
| 0.2 | 1.80 |
| 0.4 | 1.53 |
| ≥0.6 | 1.30 |

**固定开销**：系统提示 + 工具定义不在会话文件里，实测首轮约 8k–16k tokens。分析器从会话自己的数据反推：每个压缩点取「实测 prompt − 估算内容」的残差，取中位数。少了这一项，prmopt 会被低估 2 万 token 左右，正好把「有问题」看成「没问题」。

`check` 的输出就是这三层叠加后的总误差。

## 已知边界

- **`tokensBefore` 与 `after` 不可直接相减**。压缩释放多少 = `tokensBefore − after`，中间会插入 summary 和 notice 行，所以差额不等于「省下的」。要看真实释放量得看连续两轮实测 prompt 的落差。
- **`worst` 可能很大**。保留尾里若有超大 `toolResult`（几百 KB 的日志），字符估算的误差会放大；中位数不受影响，单点会偏。
- **`cost` 常为 0**。很多 provider 不返回费用，恒等于 0 时不要当真实成本用。
- **分支**：`show`/`check` 默认走最新叶节点（文件是 append-only，最后一行即当前位置）。多分支会话的其它分支没有暴露成参数。
- **`--dump`、导出 HTML、增量索引都还没做。**
- **usage 有不自洽的行**。p8 的 22,010 行 `usage` 里，61 行的 `input`/`cacheRead`/`output`
  之和与 `totalTokens` 相差几十倍（一个流式回合能报出 6,200 万 token 的 prompt），
  另 153 行干脆没有 `totalTokens`。
  所有**求和**类统计（缓存命中率、按模型的 prompt 合计、每日 prompt）都跳过这些行，
  `peak` 仍然用 `totalTokens` 兜底（它落在曲线上，是这些行里唯一没坏的数字）。
- **子代理用量不能按行相加**。`child_usage_attributed` 一行是子代理的一次调用，
  `aggregateUsage.totalTokens` 字段冻结在子会话第一次调用时（一个后来花掉 9,300 万的子会话
  至今仍报 12 万）。子会话的累计值取自最后一行 `aggregateUsage` 的各分项，并按 `targetId` 去重。

## 实测基线

在 111 个会话、418 MB 上：

| 项 | 值 |
|---|---|
| p8（183 MB / 53,591 条目 / 221 次压缩）解析 + 估算 | ~6 s |
| 全库 `list` | ~15 s |
| `check` 中位误差 | 3.7% |
| 二进制大小 | 332 KB |

## 许可

MIT
