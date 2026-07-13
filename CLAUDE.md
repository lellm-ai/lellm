## 项目愿景
- 做 Rust 版本的 LangChain / LangGraph / AutoGen
- LLM 抽象层，以及帮助快速构建常用应用的高层接口；标准化消息内容格式；提供基础的 llm provider 适配
- 低层编排层，让开发者能精准控制 Agent 的执行流程；提供基础的 function call, agent loop, tool use, mcp client/server
- 支持节点 node, 边 edge, 图 graph, Multi-Agent Orchestration
- 支持流式输出、持久化执行、短期记忆、人类介入（human-in-the-loop）

## 口号
LeLLM 传递快乐
人嘛，最重要的就是开心

## 必看
[蓝图](./docs/BLUEPRINT.md)

## 测试  Test Performance Rules
* 单个测试耗时必须 **< 10s**。
* 涉及外部调用（HTTP、MCP、DB、进程、IO 等）的测试耗时必须 **< 30s**。
* 任意测试耗时 **> 1s**，必须标注原因。

优化慢测试时：

* 优先使用 mock/fake，避免真实网络、服务、LLM 调用。
* 禁止使用长时间 `sleep` 等待，使用 `Notify`、`channel` 等事件同步方式。
* 禁止通过增加 timeout 掩盖慢测试。

运行测试时：

* 优先测试受影响 crate：

  ```bash
  cargo test -p <crate>
  ```
* 修改完成后再运行 workspace 全量测试。

测试目标：验证行为，不验证等待时间。


## 发布
- 务必使用 scripts/publish.sh
- 同时更新 README / README_zh 版本引用
