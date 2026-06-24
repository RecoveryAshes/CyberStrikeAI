---
name: lazy-js-discovery
description: Use immediately when a target is identified as an SPA or modern frontend, including Vue/React/Angular/Vite/Webpack apps, modulepreload/script chunks, hashed JS files, assets/js, static asset directories, request/core/store bundles, source maps, frontend routes, or JavaScript-discovered API endpoints. Guides browser-driven and static JS discovery so hidden routes, lazy chunks, auth flows, and backend API endpoints are not missed.
metadata:
  version: 1.0.0
  categories: [web-security, reconnaissance, attack-surface]
  requires_tools: [chrome-devtools, agent-browser]
---

# Lazy-Loaded JavaScript Discovery

## 使用边界与发散原则

- 本 skill 是最低覆盖要求、风险清单和证据标准，不是唯一测试路径或固定脚本。执行时必须根据目标技术栈、入口形态、认证状态、业务流程、已有证据、WAF/限速和授权范围调整方法。
- 文中的步骤、payload、命令、工具名和执行顺序默认是候选方法；除非本 skill、系统提示或后端门禁明确写为“必须/禁止/强制”，不要照抄固定列表，也不要因为示例没覆盖某个旁路、协议、参数位置、文件格式或业务入口就跳过。
- 发现阶段的字符串、端点、token、id、报错、组件名和扫描命中只作为候选信号；先记录和复核，不要仅凭单个关键词直接认定漏洞或加载多个无关专项流程。只有交互证据、可复现行为或明确进入对应专项测试时，才提升为专项验证。
- 每一步都应输出证据、判断理由和下一步分支；工具失败时先修参数、配置、环境或改用更合适的专业工具，确认无法覆盖时再使用通用命令兜底。
- 授权范围、低噪声、数据最小化、复现证据、误报排除和报告要求是硬边界；这些约束高于任何示例命令或默认流程。

## 最低覆盖标准

加载本 skill 只是开始，不代表已执行。对 SPA/现代前端目标，最低覆盖应包含两条线，并根据目标技术栈、认证状态、已有证据和可用工具灵活调整：

1. 手动静态提取：实际下载并解析 HTML、runtime bundle、chunk、`.js.map`，提取 API 端点、前端路由和硬编码凭证。
2. 浏览器触发懒加载：分析前端路由守卫逻辑，生成针对性 initScript 或等价浏览器脚本，用 chrome-devtools / Playwright / Puppeteer / Chromium 访问路由触发懒加载，并收集新加载的 script/network 请求。

`katana/gau/waybackurls` 适合作为专业采集补充，但不要把单一爬虫结果、只加载 skill、只看到 JS 文件、或只做 curl/grep 静态提取当成完整覆盖。进入 nuclei/sqlmap/漏洞扫描等深测前，先确认是否已具备足够的 JS/API/路由发现证据。

默认低噪声顺序建议从 `L1` 展开：`L1 无认证静态提取 -> L2 路由守卫分析与浏览器触发 -> L3 浏览器基线 -> L4 交互触发 -> L5 路由枚举 -> L6 代码分析 -> L7 Runtime Hook`。如果现场证据显示先做 L3/L4 能更快定位资源，可以调整或并行，但后续要回补 L1/L2 的证据，并说明为什么调整顺序。

## 概述

现代Web应用大量使用代码分割和懒加载优化性能，但这也导致静态扫描无法发现：
- 路由切换时才加载的admin panel
- 交互触发的隐藏API端点
- 条件加载的调试/开发功能
- SPA深层路由中的敏感操作

本skill提供系统化方法，通过自动化交互触发所有懒加载代码，确保攻击面覆盖完整。

---

## 使用场景

- ✅ 目标是SPA (React/Vue/Angular)
- ✅ 网络请求中发现chunk.[hash].js或vendors~*.js
- ✅ 页面交互后出现新功能/API调用
- ✅ 静态扫描遗漏了运行时可见的端点
- ✅ 需要发现隐藏的管理界面或调试路由

---

## 核心策略

### L1：无认证静态提取（低噪声优先，通常先做）

**背景**：大多数SPA站点即使需要登录，其JS静态资源本身是公开可访问的（只是前端路由拦截跳转到登录页）。不需要认证就能下载所有chunk文件。

**执行步骤：**

```
Step 1: 从登录页HTML和初始JS中提取所有可见JS文件URL
  - 查看HTML源码中的<script src="...">
  - 查看<link rel="modulepreload" href="...">
  - 记录所有.js文件URL

Step 2: 下载runtime/main bundle并提取webpack chunk清单
  - 下载最小的JS文件（通常是runtime~main.js，几KB）
  - 用正则提取chunk映射表：
    模式1: {123: "chunk-admin.f3d9a1"}
    模式2: e.p + "static/js/" + {101:"vendors",102:"admin"}[t] + "." + {101:"abc",102:"def"}[t] + ".js"
    模式3: __webpack_require__.u = function(e) { return "chunk." + e + ".js" }
  - 输出：所有chunk文件名列表

Step 3: 直接下载所有chunk（不需要认证！）
  - 构造完整URL：{base_url}/static/js/{chunk_filename}.js
  - 用curl/exec直接下载每个chunk文件
  - 大多数情况下返回200（因为是静态资源，不走认证）
  - 如果返回403/401，记录但继续（少数站点会保护静态资源）

Step 4: 对每个chunk文件提取API端点
  - 正则匹配：fetch("..."), axios.get("..."), "/api/..."等
  - 提取路由配置：path: "/admin", path: "/dashboard"等
  - 搜索硬编码凭证：apiKey, token, secret, password等
  - 输出：完整API列表 + 隐藏路由 + 泄露凭证

Step 5: 尝试访问Source Map
  - 对每个JS文件尝试访问 {url}.map
  - 如果map可访问，可以恢复完整源码，发现更多信息
```

**为什么这个阶段最重要：**
- 不需要登录/认证就能获取后台所有功能的API接口
- 不会触发任何WAF/IPS（只是下载静态文件）
- 即使目标只有登录页，也能提取到admin/dashboard的完整API
- webpack的chunk文件名通常在runtime.js中明文存储

**实战示例：**
```bash
# 1. 获取登录页，找到JS文件
curl -sk https://target.com/login | grep -oP 'src="[^"]+\.js"'

# 2. 下载runtime.js，提取chunk清单
curl -sk https://target.com/static/js/runtime.abc123.js | grep -oP '"[a-f0-9]{6,20}"'

# 3. 直接下载chunk（不需要cookie！）
curl -sk https://target.com/static/js/chunk-admin.f3d9a1.js -o chunk-admin.js

# 4. 从admin chunk中提取API
grep -oP 'fetch\(["\x27]/api/[^"\x27]+' chunk-admin.js
# 输出: fetch("/api/admin/users"), fetch("/api/admin/export")...
```

**关键认知：前端路由拦截≠后端JS文件访问控制**
```
前端路由拦截：
  用户访问 /admin → Vue Router检查token → 重定向到 /login
  但是！chunk-admin.js 本身仍然可以直接下载！

真正的后端保护：
  Nginx配置了 /static/js/*.js 需要认证 → 这种情况很少见
```

---

### L2：前端路由守卫绕过 + 触发懒加载（通常基于 L1 证据展开，智能生成方式）

**背景**：很多SPA站点的路由守卫会检查token，没token就跳回登录页。但跳转是前端行为，清除守卫后可以直接访问其他页面，触发懒加载JS下载。虽然API调用会返回401，但**JS文件本身会被加载**。

**核心原则：不要硬编码hook代码，先分析目标站点的JS逻辑，再生成针对性的绕过代码。**

**执行流程：**

```
Step 1: 分析目标站点的前端框架和路由机制

  1.1 从已下载的main bundle中识别框架:
      - Vue 2: 查找Vue.use(VueRouter)、new Router({的代码
      - Vue 3: 查找createRouter({、useRouter()的代码
      - React: 查找BrowserRouter、Routes、PrivateRoute的代码
      - Angular: 查找RouterModule、canActivate的代码
      - Next.js: 查找middleware、getServerSideProps的代码

  1.2 分析路由守卫的具体逻辑:
      - 它检查的是什么？localStorage.token? cookie? Vuex store?
      - 检查失败后做什么？router.push('/login')? window.location.href?
      - 守卫中是否有动态加载逻辑（如addRoute）？

  1.3 识别反调试机制:
      - 是否有无限debugger（eval/Function方式）
      - 是否检测DevTools（窗口尺寸/时间差）
      - 是否有页面跳转/关闭保护
```

```
Step 2: 根据分析结果生成针对性initScript

  根据Step 1的分析结果，动态生成最小化的绕过代码：

  示例：如果发现是Vue3 + beforeEach检查localStorage.token:
  → 生成: localStorage.setItem('token','bypass') + 清除beforeGuards

  示例：如果发现是React + PrivateRoute检查useAuth() hook:
  → 生成: 伪造auth context的返回值

  示例：如果发现有eval('debugger')反调试:
  → 生成: Hook eval替换debugger关键字

  原则：
  - 只生成必要的hook，不要一股脑全注入
  - 先试最简单的方案（如伪造token），失败再加更多hook
  - 留意守卫中的动态加载逻辑，不要简单清空导致报错
```

```
Step 3: 注入并验证效果

  3.1 用chrome-devtools_navigate_page + initScript注入生成的代码
  3.2 检查是否仍然被跳转到登录页
  3.3 如果失败，分析原因并调整代码：
      - 可能token格式不对 → 从JS中找到实际的token校验逻辑、伪造正确格式
      - 可能是服务端渲染 → 此方法无效，回退到 L1 静态提取
      - 可能有多层守卫 → 逐层清除
```

```
Step 4: 遍历路由触发懒加载

  4.1 获取路由表（通过chrome-devtools_evaluate_script）:
      - Vue: router.getRoutes() 或 router.options.routes
      - React: 从React Fiber树提取
      - 或从 L1 的 bundle 分析中已经拿到的路由列表

  4.2 逐个访问路由（守卫已清除，不会被拦截）:
      FOR EACH route IN routes:
        chrome-devtools_navigate_page({url: base_url + route})
        等待500ms
        chrome-devtools_list_network_requests({resourceTypes: ['script']})
        记录新加载的JS文件

  4.3 对新发现的JS执行API提取（同 L1 Step 4）
```

**为什么这样更好：**
- AI先理解目标站的具体实现，生成的代码更精准
- 不会因为硬编码的通用方案与目标站不兼容而失败
- 最小化注入，减少对页面的干扰
- 能处理非标准场景（自定义中间件、非Vue/React框架等）

**常见前端路由守卫模式参考（AI分析时参考）：**
```javascript
// Vue 3 - beforeEach检查localStorage
router.beforeEach((to, from, next) => {
  const token = localStorage.getItem('token');
  if (to.meta.requiresAuth && !token) {
    next('/login');
  } else {
    next();
  }
});
// 绕过方案: localStorage.setItem('token','x') 或清除beforeGuards

// Vue 2 - beforeEach检查Vuex store
router.beforeEach((to, from, next) => {
  if (to.matched.some(r => r.meta.auth) && !store.state.isAuthenticated) {
    next({name: 'Login'});
  } else {
    next();
  }
});
// 绕过方案: 清除beforeHooks[] 或伪造store.state.isAuthenticated=true

// React - PrivateRoute检查context
function PrivateRoute({children}) {
  const {isAuthenticated} = useAuth();
  return isAuthenticated ? children : <Navigate to='/login'/>;
}
// 绕过方案: 从源码找到AuthContext的初始值位置，伪造provider返回值

// Angular - canActivate guard
@Injectable()
class AuthGuard implements CanActivate {
  canActivate(): boolean {
    return this.authService.isLoggedIn();
  }
}
// 绕过方案: hook AuthService.isLoggedIn返回true
```

**为什么这样能工作：**
- 路由守卫被清除后，Vue Router不会拦截导航
- 访问 /admin 路由时，Vue会渲染Admin组件 → 触发动态import() → 加载chunk-admin.js
- 虽然Admin组件的API调用会返回401，但**JS文件已经下载到浏览器了**
- 我们只需要JS文件中的API端点信息，不需要API真正返回数据

**限制：**
- 仅对Vue/React SPA有效（服务端渲染的站点无效）
- 如果路由守卫中做了动态加载逻辑，清除守卫后可能报错（但JS文件通常仍能加载）
- 极少数站点会在服务端验证后才返回JS文件（这种情况很少见）

---

### L3：浏览器基线建立 (Baseline Capture)

**目标**: 记录初始页面加载的所有资源

```javascript
// 1. 访问目标首页
chrome-devtools_navigate_page({
  type: 'url',
  url: 'https://target.com'
})

// 2. 等待页面稳定
chrome-devtools_wait_for({
  text: ['首页关键元素'],  // 根据实际页面调整
  timeout: 10000
})

// 3. 记录基线请求
const baseline = chrome-devtools_list_network_requests({
  resourceTypes: ['script', 'fetch', 'xhr'],
  pageSize: 500
})

// 提取所有.js文件URL到baseline_scripts[]
```

**输出**:
- `baseline_scripts`: 初始加载的JS文件列表
- `baseline_apis`: 初始发现的API端点

---

### L4：交互触发 (Interaction-Driven Discovery)

**目标**: 通过自动化交互触发所有懒加载路径

#### 2.1 UI元素遍历

```markdown
MUST DO:
1. 获取页面快照: chrome-devtools_take_snapshot()
2. 提取所有可交互元素: button, a, select, input[type=checkbox/radio]
3. 对每个元素按优先级执行:
   - 导航链接/按钮 (高优先级)
   - 下拉菜单/标签页
   - 表单提交按钮
   - 悬停触发元素

FOR EACH 交互元素:
  - 执行操作 (click/hover/fill)
  - 等待500ms让请求完成
  - 调用 list_network_requests()
  - 对比baseline发现新JS文件
  - 记录: {action, element_uid, new_scripts[]}
```

#### 2.2 滚动触发

```javascript
// 无限滚动/懒加载图片可能触发新JS
chrome-devtools_evaluate_script({
  function: `async () => {
    const scrollStep = 500;
    const maxScrolls = 10;
    let scrolled = 0;

    while (scrolled < maxScrolls) {
      window.scrollBy(0, scrollStep);
      await new Promise(r => setTimeout(r, 500));

      // 检查是否到底
      if (window.scrollY + window.innerHeight >= document.body.scrollHeight) {
        break;
      }
      scrolled++;
    }

    return scrolled;
  }`
})
```

#### 2.3 表单交互触发

```markdown
对于包含表单的页面:
1. 填写表单字段触发前端验证逻辑
2. 提交表单可能加载结果页面的新模块
3. 特别关注:
   - 登录表单 (可能加载dashboard chunk)
   - 搜索框 (可能加载搜索结果组件)
   - 多步骤表单 (每步可能懒加载)
```

---

### L5：SPA路由枚举 (Route Discovery)

**目标**: 发现并访问所有SPA路由

#### 3.1 路由配置提取

```javascript
// 方法1: 检查全局路由对象
chrome-devtools_evaluate_script({
  function: `() => {
    // React Router
    if (window.__REACT_ROUTER__) return window.__REACT_ROUTER__;

    // Vue Router
    if (window.$router?.options?.routes) {
      return window.$router.options.routes.map(r => r.path);
    }

    // Angular
    if (window.ng?.getComponent) {
      // 需要进一步分析
    }

    return null;
  }`
})

// 方法2: 从打包文件中提取路由
// 下载主bundle.js，正则搜索:
// - path:\s*['"]([^'"]+)['"]
// - route:\s*['"]([^'"]+)['"]
// - {path:"(/[^"]+)"
```

#### 3.2 路由访问覆盖

```markdown
FOR EACH 发现的路由path:
  1. navigate_page({ url: `${baseUrl}${path}` })
  2. 等待页面加载完成
  3. take_snapshot() 检查是否有内容
  4. list_network_requests() 捕获新加载的chunk
  5. 如果是受保护路由(401/403)，记录下来供后续认证后测试
```

**常见隐藏路由模式**:
```
/admin
/debug
/dev
/internal
/api-docs
/swagger
/graphql
/playground
/_next/...  (Next.js内部路由)
```

---

### L6：动态代码分析 (Code Analysis)

**目标**: 从懒加载的JS中提取API端点和敏感信息

#### 4.1 下载所有新发现的JS文件

```javascript
FOR EACH new_script_url IN discovered_scripts:
  // 使用get_network_request获取响应体
  const jsCode = chrome-devtools_get_network_request({
    reqid: script_reqid,
    responseFilePath: `/tmp/lazy_${hash}.js`
  })
```

#### 4.2 静态分析提取攻击面

```python
# 使用正则或AST分析提取:

# 1. API端点
import re

api_patterns = [
    r'fetch\([\'"`]([^\'"` ]+)[\'"`]',
    r'axios\.(get|post|put|delete)\([\'"`]([^\'"` ]+)[\'"`]',
    r'XMLHttpRequest.*open\([\'"`](GET|POST)[\'"`],\s*[\'"`]([^\'"` ]+)[\'"`]',
    r'api[\'"`]:\s*[\'"`]([^\'"` ]+)[\'"`]',
]

for pattern in api_patterns:
    matches = re.findall(pattern, js_code)
    # 保存到 discovered_apis[]

# 2. 敏感字符串
sensitive_patterns = [
    r'(api[_-]?key|apikey|token|secret|password)\s*[:=]\s*[\'"`]([^\'"` ]{10,})[\'"`]',
    r'Authorization:\s*[\'"`]Bearer\s+([^\'"` ]+)[\'"`]',
]

# 3. 调试/开发功能标识
debug_keywords = ['debug', 'dev', 'admin', 'internal', '__DEV__', 'console.log']

# 4. 第三方服务配置
service_patterns = [
    r'(amazonaws\.com|s3\.)',
    r'(firebase|googleapis)\.com',
    r'(sentry|bugsnag)\.io',
]
```

#### 4.3 Source Map利用

```markdown
IF 发现 .js.map 文件:
  1. 下载source map
  2. 恢复原始源代码
  3. 分析更易读的代码逻辑
  4. 发现开发环境残留的注释和调试代码
```

---

### L7：动态Hook注入 (Runtime Interception)

**目标**: 监控运行时的动态加载行为

#### 5.1 在页面初始化时注入Hook

```javascript
chrome-devtools_navigate_page({
  url: 'https://target.com',
  initScript: `
    // Hook 1: 拦截动态import()
    window.__importedModules__ = [];
    if (window.import) {
      const origImport = window.import;
      window.import = function(url) {
        window.__importedModules__.push({
          url: url,
          timestamp: Date.now(),
          stack: new Error().stack
        });
        return origImport.apply(this, arguments);
      };
    }

    // Hook 2: 拦截fetch
    window.__fetchCalls__ = [];
    const origFetch = window.fetch;
    window.fetch = function(url, options) {
      window.__fetchCalls__.push({
        url: typeof url === 'string' ? url : url.toString(),
        method: options?.method || 'GET',
        timestamp: Date.now()
      });
      return origFetch.apply(this, arguments);
    };

    // Hook 3: 拦截动态脚本插入
    window.__dynamicScripts__ = [];
    const origCreateElement = document.createElement;
    document.createElement = function(tag) {
      const elem = origCreateElement.call(document, tag);
      if (tag.toLowerCase() === 'script') {
        const origSetSrc = Object.getOwnPropertyDescriptor(HTMLScriptElement.prototype, 'src').set;
        Object.defineProperty(elem, 'src', {
          set: function(val) {
            window.__dynamicScripts__.push(val);
            origSetSrc.call(this, val);
          }
        });
      }
      return elem;
    };
  `
})
```

#### 5.2 收集Hook数据

```javascript
// 完成所有交互后，提取监控数据
chrome-devtools_evaluate_script({
  function: `() => {
    return {
      importedModules: window.__importedModules__ || [],
      fetchCalls: window.__fetchCalls__ || [],
      dynamicScripts: window.__dynamicScripts__ || []
    };
  }`
})
```

---

## 输出清单 (Deliverables)

执行完成后，生成结构化报告:

```json
{
  "target": "https://target.com",
  "scan_time": "2026-06-02T10:30:00Z",
  "baseline": {
    "initial_scripts": 12,
    "initial_apis": 8
  },
  "discovered": {
    "lazy_scripts": [
      {
        "url": "https://target.com/static/chunk.admin.f3d9a1.js",
        "trigger": "click button[uid='nav-admin']",
        "size_kb": 145,
        "contains_apis": ["/api/admin/users", "/api/admin/settings"]
      }
    ],
    "hidden_routes": [
      "/admin/dashboard",
      "/debug/api-test",
      "/_next/webpack-hmr"
    ],
    "api_endpoints": [
      {
        "path": "/api/internal/stats",
        "method": "POST",
        "found_in": "chunk.analytics.js",
        "auth_required": true
      }
    ],
    "sensitive_findings": [
      {
        "type": "hardcoded_token",
        "value": "eyJhbGc...(truncated)",
        "location": "chunk.auth.js:line 342"
      }
    ]
  },
  "recommendations": [
    "测试 /api/internal/stats 的授权机制",
    "尝试访问 /admin/dashboard 绕过认证",
    "分析chunk.auth.js中的硬编码token"
  ]
}
```

---

## 最佳实践

### ✅ DO

- **并行化**: 同时打开多个页面测试不同路由，提高效率
- **去重**: 同一chunk文件不同hash版本只分析一次
- **记录trigger**: 记录每个懒加载文件是如何触发的，便于复现
- **深度遍历**: 对SPA要递归访问所有子路由
- **保存原始响应**: 保存所有JS文件供离线分析
- **Source Map优先**: 优先获取.map文件恢复源码

### ❌ DON'T

- **过度交互**: 避免死循环点击（如无限滚动），设置最大交互次数
- **忽略错误**: 404/500的JS请求可能揭示隐藏功能
- **只看.js**: 也要关注XHR/Fetch响应中的HTML片段（可能包含<script>）
- **跳过认证后**: 登录后可能加载完全不同的代码集合
- **遗漏WebSocket**: 某些实时功能通过WS加载配置

---

## 工具集成建议

### 与其他skill配合

```markdown
1. lazy-js-discovery (本skill)
   ↓ 输出: discovered_apis[]
2. api-security-testing
   ↓ 对每个API进行安全测试
3. xss-testing / sql-injection-testing
   ↓ 针对新发现端点的漏洞测试
4. business-logic-testing
   ↓ 测试隐藏功能的逻辑缺陷
```

### 与Burp Suite联动

```markdown
1. 使用本skill发现所有懒加载资源
2. 将discovered_apis[]导入Burp Suite
3. 使用Burp的主动扫描深入测试
4. 使用burpsuite-project-parser导出结果
```

---

## 故障排查

### 问题: 某些JS未被触发

**可能原因**:
- 需要特定设备类型（移动端/桌面端）
- 需要特定浏览器特性（WebGL/WebRTC）
- A/B测试只在部分用户启用
- 需要特定时间/地理位置

**解决方案**:
```javascript
// 模拟移动设备
chrome-devtools_emulate({
  viewport: '375x667x2,mobile,touch',
  userAgent: 'Mozilla/5.0 (iPhone; CPU iPhone OS 15_0...'
})

// 修改UA触发不同代码分支
chrome-devtools_emulate({
  userAgent: 'Googlebot/2.1 (+http://www.google.com/bot.html)'
})
```

### 问题: JS被混淆无法分析

**解决方案**:
1. 使用de4js, jsnice.org等在线工具反混淆
2. 查找source map文件
3. 使用Chrome DevTools的Pretty Print
4. 动态调试而非静态分析（使用chrome-devtools设置断点）

---

## 实战案例模板

```python
# 完整工作流示例

# Step 1: 建立基线
baseline = establish_baseline('https://target.com')

# Step 2: 自动化交互
interactions = [
    {'type': 'click', 'selector': 'nav button'},
    {'type': 'scroll', 'distance': 'bottom'},
    {'type': 'fill', 'form': 'search', 'value': 'test'},
]
discovered_scripts = trigger_interactions(interactions)

# Step 3: SPA路由枚举
routes = extract_routes_from_bundle(baseline['main_bundle'])
for route in routes:
    visit_route(route)
    new_scripts = capture_network()
    discovered_scripts.extend(new_scripts)

# Step 4: 分析新脚本
for script in discovered_scripts:
    code = download_script(script['url'])
    apis = extract_api_endpoints(code)
    secrets = scan_for_secrets(code)
    report.add(script, apis, secrets)

# Step 5: 生成报告
report.generate('lazy_js_discovery_report.json')
```

---

## 参考资料

- [OWASP Testing Guide - Client-Side Testing](https://owasp.org/www-project-web-security-testing-guide/)
- [webpack chunk splitting analysis](https://webpack.js.org/guides/code-splitting/)
- [Chrome DevTools Protocol - Network Domain](https://chromedevtools.github.io/devtools-protocol/tot/Network/)
