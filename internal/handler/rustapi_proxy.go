package handler

import (
	"context"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"strconv"
	"strings"
	"time"

	"github.com/gin-gonic/gin"
	"go.uber.org/zap"
)

const defaultRustAPIBaseURL = "http://127.0.0.1:51283"

const rustAPIStreamTimeoutSeconds = 600 * 60

type rustAPIProxyConfig struct {
	BaseURL        string
	TimeoutSeconds int
}

func rustAPIProxyConfigFromEnv() rustAPIProxyConfig {
	timeoutSeconds := 15
	if raw := strings.TrimSpace(os.Getenv("RUSTAPI_TIMEOUT_SECONDS")); raw != "" {
		if parsed, err := strconv.Atoi(raw); err == nil && parsed > 0 {
			timeoutSeconds = parsed
		}
	}
	baseURL := strings.TrimSpace(os.Getenv("RUSTAPI_BASE_URL"))
	if baseURL == "" {
		baseURL = defaultRustAPIBaseURL
	}
	return rustAPIProxyConfig{
		BaseURL:        strings.TrimRight(baseURL, "/"),
		TimeoutSeconds: timeoutSeconds,
	}
}

func rustAPITarget(cfg rustAPIProxyConfig, path string, rawQuery string) (*url.URL, error) {
	baseURL, err := url.Parse(cfg.BaseURL)
	if err != nil || baseURL.Scheme == "" || baseURL.Host == "" {
		return nil, err
	}
	target := *baseURL
	target.Path = strings.TrimRight(baseURL.Path, "/") + path
	target.RawQuery = rawQuery
	return &target, nil
}

func proxyRequestToRust(c *gin.Context, client *http.Client, path string, rawQuery string, logger *zap.Logger, label string) (int, []byte, string, bool) {
	cfg := rustAPIProxyConfigFromEnv()
	target, err := rustAPITarget(cfg, path, rawQuery)
	if err != nil {
		if logger == nil {
			logger = zap.NewNop()
		}
		logger.Error("Rust API base URL 无效", zap.String("base_url", cfg.BaseURL), zap.Error(err))
		c.JSON(http.StatusBadGateway, gin.H{"error": "Rust API base URL invalid"})
		return 0, nil, "", false
	}
	timeout := time.Duration(cfg.TimeoutSeconds) * time.Second
	ctx, cancel := context.WithTimeout(c.Request.Context(), timeout)
	defer cancel()

	var body io.Reader
	if c.Request.Body != nil {
		body = c.Request.Body
	}
	req, err := http.NewRequestWithContext(ctx, c.Request.Method, target.String(), body)
	if err != nil {
		if logger == nil {
			logger = zap.NewNop()
		}
		logger.Error("创建 Rust API 请求失败", zap.Error(err))
		c.JSON(http.StatusBadGateway, gin.H{"error": "Rust API request failed"})
		return 0, nil, "", false
	}
	copyProxyRequestHeaders(req, c.Request)

	if client == nil {
		client = http.DefaultClient
	}
	resp, err := client.Do(req)
	if err != nil {
		if logger == nil {
			logger = zap.NewNop()
		}
		if strings.TrimSpace(label) == "" {
			label = "Rust API"
		}
		logger.Error(label+"请求失败", zap.String("url", target.String()), zap.Error(err))
		c.JSON(http.StatusBadGateway, gin.H{"error": fmt.Sprintf("Rust API unavailable: %v", err)})
		return 0, nil, "", false
	}
	defer resp.Body.Close()

	respBody, err := io.ReadAll(resp.Body)
	if err != nil {
		if logger == nil {
			logger = zap.NewNop()
		}
		logger.Error("读取 Rust API 响应失败", zap.Error(err))
		c.JSON(http.StatusBadGateway, gin.H{"error": "Rust API response read failed"})
		return 0, nil, "", false
	}
	return resp.StatusCode, respBody, resp.Header.Get("Content-Type"), true
}

func proxyStreamingRequestToRust(c *gin.Context, client *http.Client, path string, rawQuery string, logger *zap.Logger, label string) bool {
	cfg := rustAPIProxyConfigFromEnv()
	target, err := rustAPITarget(cfg, path, rawQuery)
	if err != nil {
		if logger == nil {
			logger = zap.NewNop()
		}
		logger.Error("Rust API base URL 无效", zap.String("base_url", cfg.BaseURL), zap.Error(err))
		c.JSON(http.StatusBadGateway, gin.H{"error": "Rust API base URL invalid"})
		return false
	}
	timeoutSeconds := cfg.TimeoutSeconds
	if timeoutSeconds < rustAPIStreamTimeoutSeconds {
		timeoutSeconds = rustAPIStreamTimeoutSeconds
	}
	ctx, cancel := context.WithTimeout(c.Request.Context(), time.Duration(timeoutSeconds)*time.Second)
	defer cancel()

	req, err := http.NewRequestWithContext(ctx, c.Request.Method, target.String(), c.Request.Body)
	if err != nil {
		if logger == nil {
			logger = zap.NewNop()
		}
		logger.Error("创建 Rust API streaming 请求失败", zap.Error(err))
		c.JSON(http.StatusBadGateway, gin.H{"error": "Rust API request failed"})
		return false
	}
	copyProxyRequestHeaders(req, c.Request)
	req.Header.Set("Accept", "text/event-stream")

	if client == nil {
		client = http.DefaultClient
	}
	resp, err := client.Do(req)
	if err != nil {
		if logger == nil {
			logger = zap.NewNop()
		}
		if strings.TrimSpace(label) == "" {
			label = "Rust API streaming"
		}
		logger.Error(label+"请求失败", zap.String("url", target.String()), zap.Error(err))
		c.JSON(http.StatusBadGateway, gin.H{"error": fmt.Sprintf("Rust API unavailable: %v", err)})
		return false
	}
	defer resp.Body.Close()

	contentType := strings.TrimSpace(resp.Header.Get("Content-Type"))
	if contentType == "" {
		contentType = "text/event-stream; charset=utf-8"
	}
	c.Header("Content-Type", contentType)
	c.Header("Cache-Control", "no-cache, no-transform")
	c.Header("Connection", "keep-alive")
	c.Header("X-Accel-Buffering", "no")
	c.Status(resp.StatusCode)

	flusher, _ := c.Writer.(http.Flusher)
	buf := make([]byte, 32*1024)
	for {
		n, readErr := resp.Body.Read(buf)
		if n > 0 {
			if _, err := c.Writer.Write(buf[:n]); err != nil {
				return false
			}
			if flusher != nil {
				flusher.Flush()
			}
		}
		if readErr != nil {
			if readErr != io.EOF {
				if logger == nil {
					logger = zap.NewNop()
				}
				logger.Debug("Rust API streaming response closed", zap.Error(readErr))
			}
			return true
		}
	}
}

func copyProxyRequestHeaders(dst *http.Request, src *http.Request) {
	for _, name := range []string{"Accept", "Accept-Language", "Content-Type", "Last-Event-ID", "User-Agent", "X-Request-Id", "X-Forwarded-For"} {
		if value := strings.TrimSpace(src.Header.Get(name)); value != "" {
			dst.Header.Set(name, value)
		}
	}
}
