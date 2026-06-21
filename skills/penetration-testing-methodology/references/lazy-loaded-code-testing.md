# Lazy-Loaded Code Testing Guide

## 概述

针对使用代码分割(Code Splitting)、动态导入(Dynamic Import)、路由懒加载(Route-based Lazy Loading)的现代Web应用的渗透测试补充指南。

**为什么重要**:
- 静态扫描器只能看到初始HTML和同步加载的JS
- 隐藏的admin panel、debug routes、内部API常在懒加载模块中
- 硬编码凭证、敏感配置可能在未触发的chunk文件里

---

## 快速检测目标是否使用懒加载

### 特征1: 网络请求中出现chunk文件
```bash
# 访问目标后查看Network请求
# 典型文件名模式:
chunk.[hash].js
vendors~main.[hash].js
[id].[hash].chunk.js
runtime~main.[hash].js
```

### 特征2: 查看HTML中的script标签
```html
<!-- webpack动态加载标识 -->
<script src="/static/js/main.abc123.js"></script>
<!-- 注意: 主bundle很小(< 100KB)，说明大部分代码被分割了 -->
```

### 特征3: DevTools Console检查
```javascript
// 在Console执行
Object.keys(window).filter(k => k.includes('webpack'))
// 输出: ['webpackJsonp', '__webpack_require__'] 表示使用webpack

// 检查路由框架
window.$router      // Vue Router
window.__REACT_ROUTER__  // React Router
```

---

## 完整测试流程

### Phase 1: 基线捕获

**工具**: `chrome-devtools_list_network_requests`

```python
# 伪代码流程
baseline_requests = {
    'scripts': [],
    'xhr': [],
    'fetch': []
}

# 1. 访问目标首页
navigate_to('https://target.com')
wait_for_load()

# 2. 记录所有初始请求
all_requests = list_network_requests(resourceTypes=['script', 'xhr', 'fetch'])

for req in all_requests:
    baseline_requests[req.type].append({
        'url': req.url,
        'size': req.response_size,
        'status': req.status_code
    })

# 3. 保存baseline供后续对比
save_json('baseline.json', baseline_requests)
```

**输出示例**:
```json
{
  "scripts": [
    {"url": "/static/js/main.f4a3.js", "size": 89234},
    {"url": "/static/js/runtime.1e2d.js", "size": 2341}
  ],
  "fetch": [
    {"url": "/api/config", "status": 200}
  ]
}
```

---

### Phase 2: 自动化交互触发

**策略**: 系统化点击所有可交互元素

```python
# 1. 获取页面快照
snapshot = take_snapshot()

# 2. 提取所有交互元素
interactable_elements = extract_from_snapshot(snapshot, types=[
    'button',
    'a',           # 链接
    'select',      # 下拉菜单
    '[role=tab]',  # 标签页
    '[role=menuitem]'
])

# 3. 按优先级排序
prioritized = sort_by_priority(interactable_elements, rules=[
    ('导航类', ['nav', 'menu', 'header']),      # 最高优先级
    ('功能类', ['admin', 'settings', 'profile']),
    ('其他', ['*'])
])

# 4. 依次触发并监控
discovered_lazy_scripts = []

for element in prioritized:
    # 记录当前网络状态
    before_requests = list_network_requests()

    # 执行交互
    if element.type == 'button':
        click(element.uid)
    elif element.type == 'a':
        click(element.uid)
    elif element.type == 'select':
        fill(element.uid, value='any_option')

    # 等待新请求完成
    wait(500)  # ms

    # 检测新增请求
    after_requests = list_network_requests()
    new_scripts = diff_requests(after_requests, before_requests, type='script')

    if new_scripts:
        discovered_lazy_scripts.append({
            'trigger': element.text or element.uid,
            'scripts': new_scripts
        })

# 5. 滚动触发
for scroll_position in [500, 1000, 'bottom']:
    scroll_to(scroll_position)
    wait(500)
    # 同样的diff逻辑
```

**真实案例**:
```json
{
  "trigger": "button[uid='admin-panel']",
  "scripts": [
    {
      "url": "/static/js/chunk-admin.3f4a.js",
      "size": 145000,
      "loaded_at": "2026-06-02T10:35:22Z"
    }
  ]
}
```

---

### Phase 3: SPA路由暴力枚举

#### 方法A: 从Bundle提取路由配置

```python
# 1. 下载主bundle文件
main_bundle_url = baseline_requests['scripts'][0]['url']
bundle_code = download_js(main_bundle_url)

# 2. 正则提取路由定义
route_patterns = [
    r'path:\s*[\'"`]([^\'"` ]+)[\'"`]',           # path: "/admin"
    r'route:\s*[\'"`]([^\'"` ]+)[\'"`]',          # route: "/settings"
    r'\{\s*path:\s*[\'"`]([^\'"` ]+)[\'"`]',      # {path: "/users"}
    r'RouteConfig.*?[\'"`]([/\w-]+)[\'"`]',       # RouteConfig定义
]

discovered_routes = []
for pattern in route_patterns:
    matches = re.findall(pattern, bundle_code, re.IGNORECASE)
    discovered_routes.extend(matches)

# 3. 去重并过滤
discovered_routes = list(set(discovered_routes))
discovered_routes = [r for r in discovered_routes if r.startswith('/')]

# 输出: ['/admin', '/settings', '/profile', '/debug']
```

#### 方法B: 运行时提取

```javascript
// 在DevTools Console注入
evaluate_script({
  function: `() => {
    // Vue Router
    if (window.$router?.options?.routes) {
      return window.$router.options.routes.map(r => ({
        path: r.path,
        name: r.name,
        component: r.component?.name || 'unknown'
      }));
    }

    // React Router v6
    if (window.__reactRouterVersion) {
      // 需要从组件树提取
    }

    // Next.js
    if (window.__NEXT_DATA__?.props?.pageProps) {
      return Object.keys(window.__NEXT_DATA__.props.pageProps);
    }

    return null;
  }`
})
```

#### 方法C: 常见隐藏路由字典

```python
common_hidden_routes = [
    # 管理界面
    '/admin', '/admin/', '/administrator', '/admin/dashboard',
    '/admin/login', '/admin/console', '/backend',

    # 调试/开发
    '/debug', '/dev', '/test', '/__debug__', '/internal',
    '/playground', '/_next/webpack-hmr',

    # API文档
    '/api-docs', '/swagger', '/docs', '/api/docs',
    '/graphql', '/graphiql', '/playground',

    # 监控/状态
    '/health', '/status', '/metrics', '/stats',
    '/_health', '/actuator',

    # 框架特定
    '/.well-known/', '/.git/config', '/node_modules/',
]

# 逐个访问并检测响应
for route in common_hidden_routes:
    response = navigate_to(f'{target_url}{route}')
    if response.status == 200:
        # 成功访问,记录
        # 检查是否加载了新的chunk
        new_chunks = detect_new_scripts()
```

#### 执行访问

```python
# 对所有发现的路由逐个访问
for route in discovered_routes + common_hidden_routes:
    try:
        navigate_to(f'{base_url}{route}')
        wait_for_load(timeout=5000)

        # 检查是否有效路由
        snapshot = take_snapshot()
        if not is_404_page(snapshot):
            # 记录新加载的脚本
            new_scripts = diff_network_requests()
            report_route(route, new_scripts)

    except TimeoutError:
        # 某些路由可能需要认证,记录下来
        protected_routes.append(route)
```

---

### Phase 4: 代码分析提取攻击面

对每个新发现的JS文件进行静态分析:

```python
def analyze_lazy_script(script_url, script_code):
    findings = {
        'api_endpoints': [],
        'sensitive_strings': [],
        'third_party_services': [],
        'debug_code': []
    }

    # 1. 提取API端点
    api_patterns = [
        (r'fetch\([\'"`](/[^\'"` ]+)[\'"`]', 'fetch'),
        (r'axios\.(get|post|put|delete)\([\'"`]([^\'"` ]+)[\'"`]', 'axios'),
        (r'\.ajax\(\{[^}]*url:\s*[\'"`]([^\'"` ]+)[\'"`]', 'jQuery.ajax'),
        (r'new\s+XMLHttpRequest.*open\([\'"`]\w+[\'"`],\s*[\'"`]([^\'"` ]+)[\'"`]', 'XHR'),
    ]

    for pattern, method in api_patterns:
        matches = re.findall(pattern, script_code, re.IGNORECASE)
        for match in matches:
            endpoint = match if isinstance(match, str) else match[-1]
            findings['api_endpoints'].append({
                'endpoint': endpoint,
                'method': method,
                'found_in': script_url
            })

    # 2. 提取敏感字符串
    sensitive_patterns = [
        (r'(api[_-]?key|apikey)\s*[:=]\s*[\'"`]([^\'"` ]{20,})[\'"`]', 'API Key'),
        (r'(access[_-]?token|accesstoken)\s*[:=]\s*[\'"`]([^\'"` ]{20,})[\'"`]', 'Access Token'),
        (r'(secret|password)\s*[:=]\s*[\'"`]([^\'"` ]{8,})[\'"`]', 'Secret/Password'),
        (r'Bearer\s+([A-Za-z0-9\-_]+\.[A-Za-z0-9\-_]+\.[A-Za-z0-9\-_]+)', 'JWT Token'),
        (r'(AKIA[0-9A-Z]{16})', 'AWS Access Key'),
        (r'(ghp_[a-zA-Z0-9]{36})', 'GitHub Token'),
    ]

    for pattern, type_name in sensitive_patterns:
        matches = re.findall(pattern, script_code, re.IGNORECASE)
        for match in matches:
            findings['sensitive_strings'].append({
                'type': type_name,
                'value': match[-1][:50] + '...',  # 截断避免泄露完整值
                'location': script_url
            })

    # 3. 检测第三方服务
    service_patterns = [
        (r'https?://[^/]*\.amazonaws\.com', 'AWS S3'),
        (r'https?://[^/]*\.googleapis\.com', 'Google APIs'),
        (r'https?://[^/]*\.firebase(io|app)\.com', 'Firebase'),
        (r'https?://[^/]*\.sentry\.io', 'Sentry'),
    ]

    for pattern, service in service_patterns:
        if re.search(pattern, script_code):
            findings['third_party_services'].append(service)

    # 4. 查找调试代码
    debug_keywords = ['console.log', '__DEV__', 'debugger;', 'debug:', 'DEBUG']
    for keyword in debug_keywords:
        if keyword in script_code:
            # 提取上下文
            lines = script_code.split('\n')
            for i, line in enumerate(lines):
                if keyword in line:
                    findings['debug_code'].append({
                        'line': i + 1,
                        'code': line.strip()[:100]
                    })

    return findings

# 对所有lazy scripts执行分析
for script in discovered_lazy_scripts:
    code = download_js(script['url'])
    analysis = analyze_lazy_script(script['url'], code)
    save_analysis(script['url'], analysis)
```

**分析输出示例**:
```json
{
  "script": "/static/js/chunk-admin.3f4a.js",
  "findings": {
    "api_endpoints": [
      {
        "endpoint": "/api/admin/users/export",
        "method": "fetch",
        "risk": "可能存在IDOR或信息泄露"
      },
      {
        "endpoint": "/api/internal/debug/logs",
        "method": "axios",
        "risk": "内部调试端点暴露"
      }
    ],
    "sensitive_strings": [
      {
        "type": "AWS Access Key",
        "value": "AKIAIOSFODNN7EXAMPLE...",
        "risk": "硬编码AWS凭证"
      }
    ],
    "debug_code": [
      {
        "line": 342,
        "code": "console.log('Admin auth token:', token);",
        "risk": "Token泄露到Console"
      }
    ]
  }
}
```

---

### Phase 5: Runtime Hook监控

在页面加载前注入监控代码:

```javascript
// 使用chrome-devtools_navigate_page的initScript参数
const monitoring_script = `
(function() {
  // 创建全局监控对象
  window.__securityMonitor__ = {
    imports: [],
    fetches: [],
    dynamicScripts: [],
    storageAccess: []
  };

  // Hook 1: 动态import()
  if (typeof window.import !== 'undefined') {
    const origImport = window.import;
    window.import = function(url) {
      window.__securityMonitor__.imports.push({
        url: url,
        timestamp: new Date().toISOString(),
        stack: new Error().stack.split('\\n').slice(2, 5).join('\\n')
      });
      return origImport.apply(this, arguments);
    };
  }

  // Hook 2: Fetch API
  const origFetch = window.fetch;
  window.fetch = function(url, options) {
    const urlStr = typeof url === 'string' ? url : url.toString();
    window.__securityMonitor__.fetches.push({
      url: urlStr,
      method: options?.method || 'GET',
      headers: options?.headers || {},
      timestamp: new Date().toISOString()
    });

    // 如果请求包含敏感header,记录
    if (options?.headers?.Authorization) {
      console.warn('[Security] Auth header sent to:', urlStr);
    }

    return origFetch.apply(this, arguments);
  };

  // Hook 3: XMLHttpRequest
  const origXHROpen = XMLHttpRequest.prototype.open;
  XMLHttpRequest.prototype.open = function(method, url) {
    window.__securityMonitor__.fetches.push({
      url: url,
      method: method,
      type: 'XHR',
      timestamp: new Date().toISOString()
    });
    return origXHROpen.apply(this, arguments);
  };

  // Hook 4: 动态脚本插入
  const origCreateElement = document.createElement;
  document.createElement = function(tagName) {
    const element = origCreateElement.call(document, tagName);

    if (tagName.toLowerCase() === 'script') {
      const descriptor = Object.getOwnPropertyDescriptor(HTMLScriptElement.prototype, 'src');
      if (descriptor && descriptor.set) {
        const origSrcSetter = descriptor.set;
        Object.defineProperty(element, 'src', {
          set: function(url) {
            window.__securityMonitor__.dynamicScripts.push({
              url: url,
              timestamp: new Date().toISOString()
            });
            console.info('[Security] Dynamic script loaded:', url);
            return origSrcSetter.call(this, url);
          },
          get: descriptor.get
        });
      }
    }

    return element;
  };

  // Hook 5: LocalStorage/SessionStorage访问
  const hookStorage = (storage, name) => {
    const origSetItem = storage.setItem;
    const origGetItem = storage.getItem;

    storage.setItem = function(key, value) {
      window.__securityMonitor__.storageAccess.push({
        storage: name,
        action: 'setItem',
        key: key,
        timestamp: new Date().toISOString()
      });

      // 检测敏感数据
      if (/(token|password|secret|key|auth)/i.test(key)) {
        console.warn(\`[Security] Sensitive data stored in \${name}:\`, key);
      }

      return origSetItem.call(this, key, value);
    };

    storage.getItem = function(key) {
      window.__securityMonitor__.storageAccess.push({
        storage: name,
        action: 'getItem',
        key: key,
        timestamp: new Date().toISOString()
      });
      return origGetItem.call(this, key);
    };
  };

  hookStorage(localStorage, 'localStorage');
  hookStorage(sessionStorage, 'sessionStorage');

  console.info('[Security Monitor] Initialized successfully');
})();
`;

// 访问目标时注入
chrome-devtools_navigate_page({
  url: 'https://target.com',
  initScript: monitoring_script
});

// ... 执行所有交互 ...

// 完成后提取监控数据
const monitor_data = chrome-devtools_evaluate_script({
  function: `() => window.__securityMonitor__`
});

// 分析监控结果
analyze_monitor_data(monitor_data);
```

**监控数据示例**:
```json
{
  "imports": [
    {
      "url": "./chunk-admin.js",
      "timestamp": "2026-06-02T10:35:25.123Z",
      "stack": "at AdminPanel.loadModule\\nat Router.navigate"
    }
  ],
  "fetches": [
    {
      "url": "/api/admin/users",
      "method": "GET",
      "headers": {"Authorization": "Bearer eyJhbGc..."},
      "timestamp": "2026-06-02T10:35:26.456Z"
    }
  ],
  "dynamicScripts": [
    {
      "url": "https://cdn.example.com/analytics.js",
      "timestamp": "2026-06-02T10:35:24.789Z"
    }
  ],
  "storageAccess": [
    {
      "storage": "localStorage",
      "action": "setItem",
      "key": "auth_token",
      "timestamp": "2026-06-02T10:35:23.012Z"
    }
  ]
}
```

---

## 高级技巧

### Source Map利用

```python
# 1. 检测是否存在source map
if '//# sourceMappingURL=' in script_code:
    map_url = extract_sourcemap_url(script_code)

    # 2. 下载source map
    source_map = download_json(map_url)

    # 3. 恢复原始源码
    original_sources = decode_sourcemap(source_map)

    # 4. 分析原始代码(更易读)
    for source_file, source_code in original_sources.items():
        if 'admin' in source_file or 'internal' in source_file:
            # 优先分析敏感模块
            analyze_source(source_file, source_code)
```

### Webpack Chunk清单分析

```python
# webpack的chunk清单包含所有异步模块信息
# 通常在runtime~main.js中

def extract_chunk_manifest(runtime_code):
    # 查找类似: {123: "chunk-admin.js", 456: "chunk-user.js"}
    manifest_pattern = r'\{([0-9]+):\s*[\'"`]([^\'"` ]+\.js)[\'"`]'
    chunks = re.findall(manifest_pattern, runtime_code)

    return [
        {'id': chunk_id, 'filename': filename}
        for chunk_id, filename in chunks
    ]

# 遍历清单中的所有chunk
manifest = extract_chunk_manifest(runtime_code)
for chunk in manifest:
    # 构造完整URL
    chunk_url = f"{base_url}/static/js/{chunk['filename']}"
    # 尝试下载(即使未被触发加载)
    try:
        chunk_code = download_js(chunk_url)
        analyze_lazy_script(chunk_url, chunk_code)
    except HTTPError:
        pass  # chunk可能不存在或受保护
```

### 绕过反调试

某些应用检测DevTools开启状态:

```javascript
// 检测方法
(function() {
  const devtools = /./;
  devtools.toString = function() {
    this.opened = true;
  };
  console.log('%c', devtools);
  if (devtools.opened) {
    // 触发反调试逻辑
  }
})();

// 绕过方法: 使用Headless模式 + 协议级操作
chrome-devtools_emulate({
  // 不要实际打开DevTools UI
  // 仅通过CDP协议操作
});
```

---

## 输出报告模板

```markdown
# 懒加载代码发现报告

## 目标信息
- URL: https://target.com
- 扫描时间: 2026-06-02 10:30:00
- 框架: React 18 + webpack 5

## 发现统计
- 初始JS文件: 12个 (总计890KB)
- 懒加载JS文件: 23个 (总计2.1MB)
- 新发现API端点: 47个
- 隐藏路由: 8个
- 敏感发现: 3个

## 关键发现

### 1. 隐藏管理界面
**路由**: `/admin/dashboard`
**触发方式**: 点击导航栏"管理"按钮
**加载文件**: `chunk-admin.f3d9a1.js` (145KB)
**风险**:
- 包含用户导出功能 `/api/admin/users/export`
- 无前端权限检查,依赖后端验证
- 建议测试: IDOR, 批量导出

### 2. 硬编码AWS凭证
**文件**: `chunk-upload.a2c4b3.js`
**位置**: 第247行
**内容**:
```javascript
const s3Config = {
  accessKeyId: 'AKIAIOSFODNN7EXAMPLE',
  secretAccessKey: 'wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY',
  bucket: 'company-uploads'
};
```
**风险**: 高危 - AWS凭证泄露,可直接访问S3存储桶

### 3. 调试端点暴露
**端点**: `/api/internal/debug/logs`
**发现于**: `chunk-devtools.e8f1c2.js`
**风险**:
- 未在生产环境移除调试代码
- 可能泄露敏感日志信息
- 建议测试: 未授权访问

## 完整清单

### 懒加载文件列表
| 文件 | 大小 | 触发方式 | 包含API数 |
|------|------|----------|-----------|
| chunk-admin.f3d9a1.js | 145KB | 点击"管理" | 12 |
| chunk-upload.a2c4b3.js | 67KB | 上传文件 | 3 |
| chunk-analytics.9b7e5a.js | 234KB | 访问/analytics | 8 |

### 发现的API端点
| 端点 | 方法 | 发现位置 | 状态 |
|------|------|----------|------|
| /api/admin/users/export | GET | chunk-admin.js | 未测试 |
| /api/internal/debug/logs | POST | chunk-devtools.js | 未测试 |
| /api/s3/presigned-url | GET | chunk-upload.js | 未测试 |

### 隐藏路由
| 路由 | 状态码 | 加载chunk | 备注 |
|------|--------|-----------|------|
| /admin/dashboard | 200 | chunk-admin.js | 需认证 |
| /debug/api-test | 403 | - | 被阻止 |
| /_internal/stats | 200 | chunk-internal.js | 无认证! |

## 后续测试建议

1. **优先级P0**:
   - 测试 `/api/admin/users/export` 的IDOR漏洞
   - 验证硬编码的AWS凭证有效性
   - 尝试未授权访问 `/_internal/stats`

2. **优先级P1**:
   - 测试所有新发现API的授权机制
   - 检查隐藏路由的访问控制
   - 分析chunk-upload.js的文件上传逻辑

3. **优先级P2**:
   - 深入分析source map恢复的源码
   - 测试动态加载的第三方库是否有已知漏洞
   - 检查localStorage中存储的敏感数据

## 工具使用记录
- Chrome DevTools Protocol
- CyberStrikeAI lazy-js-discovery skill
- 自定义监控脚本(Hook Injection)

---
报告生成时间: 2026-06-02 11:45:00
```

---

## 故障排查

### 问题: Hook未生效

**症状**: `window.__securityMonitor__` 为undefined

**排查**:
```javascript
// 检查initScript是否正确注入
chrome-devtools_evaluate_script({
  function: `() => {
    return {
      hasMonitor: typeof window.__securityMonitor__ !== 'undefined',
      hasOrigFetch: typeof window.fetch !== 'undefined'
    };
  }`
});

// 如果未注入,手动执行
chrome-devtools_evaluate_script({
  function: monitoring_script  // 前面定义的完整脚本
});
```

### 问题: Source Map 404

**原因**: 生产环境通常删除.map文件

**解决方案**:
1. 尝试访问常见路径: `/static/js/*.map`, `/dist/*.map`
2. 检查JS文件末尾是否有sourceMappingURL注释
3. 使用反混淆工具: jsnice.org, de4js.com

### 问题: 某些chunk无法触发

**原因**: 需要特定条件(认证/特定设备/A/B测试)

**解决方案**:
```python
# 1. 登录后重新执行扫描
login_to_application()
repeat_discovery_phases()

# 2. 模拟不同设备
for ua in ['Mobile', 'Desktop', 'Tablet']:
    chrome-devtools_emulate({userAgent: ua})
    repeat_discovery_phases()

# 3. 直接从webpack清单下载所有chunk(绕过触发)
manifest = extract_chunk_manifest()
for chunk in manifest:
    try:
        download_and_analyze(chunk['url'])
    except:
        pass
```

---

## 参考资料

- [webpack Code Splitting](https://webpack.js.org/guides/code-splitting/)
- [Chrome DevTools Protocol - Network](https://chromedevtools.github.io/devtools-protocol/tot/Network/)
- [Source Map Specification](https://sourcemaps.info/spec.html)
- [OWASP Testing Guide - Client-Side](https://owasp.org/www-project-web-security-testing-guide/)
