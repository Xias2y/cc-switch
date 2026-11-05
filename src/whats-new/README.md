# 更新摘要（应用内弹窗）

用户升级后第一次打开 CC Switch，会弹一个窗口，列出从上次看过的版本到当前版本之间每一版的几条摘要。数据就是这个目录下的文件，构建时打进安装包（`src/lib/whatsNew.ts`），弹窗不联网。

详细内容仍在 `docs/release-notes/`。本分支的「详情」链接到 `Xias2y/cc-switch` 对应的 GitHub Release（例如 `4.0.26-krill.3` 对应 `krill-v4.0.26-3`）；0.1.0 稳定版统一使用版本 `0.1.0`，链接到 `krill-v0.1.0`；没有对应定制版本时打开发布列表。发布说明中文在前、英文在后，无需同步上游官网。

## 每个版本一个文件

文件名是版本号，例如 `4.0.2.json`：

```json
{
  "version": "4.0.2",
  "items": [
    {
      "type": "new",
      "zh": "OpenCode 可以选择思考档位",
      "zh-TW": "OpenCode 可以選擇思考檔位",
      "en": "OpenCode can now pick a thinking level",
      "ja": "OpenCode で思考レベルを選べるように"
    }
  ]
}
```

- `type`：`new`（新增）、`fix`（修复）、`improve`（改进）。
- 每版最多 4 条，每条一句话：中文、繁中不超过 40 字，日文不超过 50 字，英文不超过 100 个字符。四种语言都要写。
- 写用户能感知到的变化，按影响大小排序。从中文发布说明里挑最重要的几条，不要照搬原文。
- `"items": []` 表示这一版不弹窗，适合纯构建、CI 之类的版本。文件仍然要有。

规则由 `tests/config/whatsNewEntries.test.ts` 检查。

## 什么时候写

和三语发布说明一起，放进打 tag 之前的 `docs(release)` 提交。`release.yml` 会先检查 `src/whats-new/<版本>.json` 是否存在，缺了直接失败，不会白跑构建。
