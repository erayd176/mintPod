package main

import (
	"context"
	"crypto/subtle"
	"encoding/json"
	"errors"
	"log"
	"net/http"
	"net/http/httputil"
	"net/url"
	"os"
	"os/exec"
	"os/signal"
	"strings"
	"syscall"
	"time"
)

const (
	defaultListenAddress = ":8000"
	defaultOllamaURL     = "http://127.0.0.1:11434"
)

func main() {
	token := strings.TrimSpace(os.Getenv("MINTPOD_RUNTIME_TOKEN"))
	if token == "" {
		log.Fatal("MINTPOD_RUNTIME_TOKEN is required")
	}

	ollamaURL, err := url.Parse(envOrDefault("MINTPOD_OLLAMA_URL", defaultOllamaURL))
	if err != nil {
		log.Fatalf("invalid MINTPOD_OLLAMA_URL: %v", err)
	}

	ctx, cancel := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer cancel()

	ollama := exec.CommandContext(ctx, "ollama", "serve")
	ollama.Stdout = os.Stdout
	ollama.Stderr = os.Stderr
	if err := ollama.Start(); err != nil {
		log.Fatalf("start Ollama: %v", err)
	}

	proxy := authenticatedProxy(ollamaURL, token)
	server := &http.Server{
		Addr:              envOrDefault("MINTPOD_LISTEN_ADDRESS", defaultListenAddress),
		Handler:           proxy,
		ReadHeaderTimeout: 10 * time.Second,
		IdleTimeout:       5 * time.Minute,
	}

	serverDone := make(chan error, 1)
	go func() {
		log.Printf("mintPod runtime listening on %s", server.Addr)
		serverDone <- server.ListenAndServe()
	}()

	select {
	case <-ctx.Done():
	case err := <-serverDone:
		if !errors.Is(err, http.ErrServerClosed) {
			log.Printf("runtime gateway failed: %v", err)
		}
		cancel()
	}

	shutdownCtx, shutdownCancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer shutdownCancel()
	_ = server.Shutdown(shutdownCtx)
	if ollama.Process != nil {
		_ = ollama.Process.Signal(syscall.SIGTERM)
	}
	_ = ollama.Wait()
}

func authenticatedProxy(target *url.URL, token string) http.Handler {
	proxy := httputil.NewSingleHostReverseProxy(target)
	originalDirector := proxy.Director
	proxy.Director = func(request *http.Request) {
		originalDirector(request)
		request.Header.Del("Authorization")
		request.Header.Del("Proxy-Authorization")
	}
	proxy.ErrorHandler = func(response http.ResponseWriter, _ *http.Request, err error) {
		log.Printf("Ollama unavailable: %v", err)
		writeJSONError(response, http.StatusBadGateway, "model runtime unavailable")
	}

	return http.HandlerFunc(func(response http.ResponseWriter, request *http.Request) {
		if !validBearerToken(request.Header.Get("Authorization"), token) {
			writeJSONError(response, http.StatusUnauthorized, "invalid runtime API key")
			return
		}
		proxy.ServeHTTP(response, request)
	})
}

func validBearerToken(header, token string) bool {
	const prefix = "Bearer "
	if !strings.HasPrefix(header, prefix) {
		return false
	}
	provided := strings.TrimPrefix(header, prefix)
	return subtle.ConstantTimeCompare([]byte(provided), []byte(token)) == 1
}

func writeJSONError(response http.ResponseWriter, status int, message string) {
	response.Header().Set("Content-Type", "application/json")
	response.WriteHeader(status)
	_ = json.NewEncoder(response).Encode(map[string]string{"error": message})
}

func envOrDefault(name, fallback string) string {
	if value := strings.TrimSpace(os.Getenv(name)); value != "" {
		return value
	}
	return fallback
}
