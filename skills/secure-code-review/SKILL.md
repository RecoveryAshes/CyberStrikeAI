---
name: secure-code-review
description: 安全代码审查的专业技能和方法论
version: 1.0.0
---

# 安全代码审查

## 使用边界与发散原则

- 本 skill 是最低覆盖要求、风险清单和证据标准，不是唯一测试路径或固定脚本。执行时必须根据目标技术栈、入口形态、认证状态、业务流程、已有证据、WAF/限速和授权范围调整方法。
- 文中的步骤、payload、命令、工具名和执行顺序默认是候选方法；除非本 skill、系统提示或后端门禁明确写为“必须/禁止/强制”，不要照抄固定列表，也不要因为示例没覆盖某个旁路、协议、参数位置、文件格式或业务入口就跳过。
- 发现阶段的字符串、端点、token、id、报错、组件名和扫描命中只作为候选信号；先记录和复核，不要仅凭单个关键词直接认定漏洞或加载多个无关专项流程。只有交互证据、可复现行为或明确进入对应专项测试时，才提升为专项验证。
- 每一步都应输出证据、判断理由和下一步分支；工具失败时先修参数、配置、环境或改用更合适的专业工具，确认无法覆盖时再使用通用命令兜底。
- 授权范围、低噪声、数据最小化、复现证据、误报排除和报告要求是硬边界；这些约束高于任何示例命令或默认流程。

## 概述

安全代码审查是识别代码中安全漏洞的重要方法。本技能提供安全代码审查的方法、工具和最佳实践。

## 审查范围

### 1. 输入验证

**检查项目：**
- 用户输入验证
- 参数验证
- 数据过滤
- 边界检查

### 2. 输出编码

**检查项目：**
- XSS防护
- 输出编码
- 内容安全策略
- 响应头设置

### 3. 认证授权

**检查项目：**
- 认证机制
- 会话管理
- 权限控制
- 密码处理

### 4. 加密和密钥

**检查项目：**
- 数据加密
- 密钥管理
- 哈希算法
- 随机数生成

## 审查方法

### 1. 静态分析

**使用SAST工具：**
```bash
# SonarQube
sonar-scanner

# Checkmarx
# 使用Web界面

# Fortify
sourceanalyzer -b project build.sh
sourceanalyzer -b project -scan

# Semgrep
semgrep --config=auto .
```

### 2. 手动审查

**审查清单：**
- [ ] 输入验证
- [ ] 输出编码
- [ ] SQL注入
- [ ] XSS漏洞
- [ ] 认证授权
- [ ] 加密使用
- [ ] 错误处理
- [ ] 日志记录

### 3. 代码模式识别

**危险函数：**
```python
# Python危险函数
eval()
exec()
pickle.loads()
os.system()
subprocess.call()
```

```java
// Java危险函数
Runtime.exec()
ProcessBuilder()
Class.forName()
```

```php
// PHP危险函数
eval()
exec()
system()
passthru()
```

## 常见漏洞模式

### SQL注入

**危险代码：**
```java
String query = "SELECT * FROM users WHERE id = " + userId;
Statement stmt = connection.createStatement();
ResultSet rs = stmt.executeQuery(query);
```

**安全代码：**
```java
String query = "SELECT * FROM users WHERE id = ?";
PreparedStatement stmt = connection.prepareStatement(query);
stmt.setInt(1, userId);
ResultSet rs = stmt.executeQuery();
```

### XSS漏洞

**危险代码：**
```javascript
document.innerHTML = userInput;
element.innerHTML = "<div>" + userInput + "</div>";
```

**安全代码：**
```javascript
element.textContent = userInput;
element.setAttribute("data-value", userInput);
// 或使用编码库
element.innerHTML = escapeHtml(userInput);
```

### 命令注入

**危险代码：**
```python
import os
os.system("ping " + user_input)
```

**安全代码：**
```python
import subprocess
subprocess.run(["ping", "-c", "1", validated_input])
```

### 路径遍历

**危险代码：**
```java
String filePath = "/uploads/" + fileName;
File file = new File(filePath);
```

**安全代码：**
```java
String basePath = "/uploads/";
String fileName = Paths.get(fileName).getFileName().toString();
String filePath = basePath + fileName;
File file = new File(filePath);
if (!file.getCanonicalPath().startsWith(basePath)) {
    throw new SecurityException("Invalid path");
}
```

### 硬编码密钥

**危险代码：**
```java
String apiKey = "1234567890abcdef";
String password = "admin123";
```

**安全代码：**
```java
String apiKey = System.getenv("API_KEY");
String password = keyStore.getPassword("db_password");
```

## 工具使用

### SonarQube

```bash
# 启动SonarQube
docker run -d -p 9000:9000 sonarqube

# 运行扫描
sonar-scanner \
  -Dsonar.projectKey=myproject \
  -Dsonar.sources=. \
  -Dsonar.host.url=http://localhost:9000
```

### Semgrep

```bash
# 安装
pip install semgrep

# 运行扫描
semgrep --config=auto .

# 使用规则
semgrep --config=p/security-audit .
```

### CodeQL

```bash
# 创建数据库
codeql database create database --language=java --source-root=.

# 运行查询
codeql database analyze database security-and-quality.qls --format=sarif-latest
```

## 审查清单

### 输入验证
- [ ] 所有用户输入都经过验证
- [ ] 使用白名单验证
- [ ] 验证数据类型和范围
- [ ] 处理特殊字符

### 输出编码
- [ ] HTML输出编码
- [ ] URL编码
- [ ] JavaScript编码
- [ ] SQL参数化

### 认证授权
- [ ] 强密码策略
- [ ] 安全的会话管理
- [ ] 权限验证
- [ ] 多因素认证

### 加密
- [ ] 使用强加密算法
- [ ] 密钥安全存储
- [ ] 传输加密
- [ ] 存储加密

### 错误处理
- [ ] 不泄露敏感信息
- [ ] 统一错误响应
- [ ] 记录错误日志
- [ ] 异常处理

## 最佳实践

### 1. 安全编码规范

- 遵循OWASP Top 10
- 使用安全编码指南
- 代码审查流程
- 安全培训

### 2. 自动化工具

- 集成SAST工具
- CI/CD安全检查
- 自动化扫描
- 结果分析

### 3. 代码审查流程

- 同行审查
- 安全专家审查
- 定期审查
- 记录问题

## 注意事项

- 结合工具和人工审查
- 关注业务逻辑漏洞
- 定期更新工具规则
- 建立安全编码文化