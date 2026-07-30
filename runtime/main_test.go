package main

import (
	"io"
	"net/http"
	"net/http/httptest"
	"net/url"
	"testing"
)

func TestGatewayRequiresExactBearerToken(t *testing.T) {
	upstream := httptest.NewServer(http.HandlerFunc(func(response http.ResponseWriter, _ *http.Request) {
		response.WriteHeader(http.StatusNoContent)
	}))
	defer upstream.Close()
	target, _ := url.Parse(upstream.URL)
	gateway := httptest.NewServer(authenticatedProxy(target, "correct"))
	defer gateway.Close()

	for _, header := range []string{"", "correct", "Bearer wrong"} {
		request, _ := http.NewRequest(http.MethodGet, gateway.URL+"/api/version", nil)
		request.Header.Set("Authorization", header)
		response, err := http.DefaultClient.Do(request)
		if err != nil {
			t.Fatal(err)
		}
		_ = response.Body.Close()
		if response.StatusCode != http.StatusUnauthorized {
			t.Fatalf("header %q returned %d", header, response.StatusCode)
		}
	}

	request, _ := http.NewRequest(http.MethodGet, gateway.URL+"/api/version", nil)
	request.Header.Set("Authorization", "Bearer correct")
	response, err := http.DefaultClient.Do(request)
	if err != nil {
		t.Fatal(err)
	}
	_ = response.Body.Close()
	if response.StatusCode != http.StatusNoContent {
		t.Fatalf("valid token returned %d", response.StatusCode)
	}
}

func TestGatewayStripsCredentialBeforeOllama(t *testing.T) {
	authorization := make(chan string, 1)
	upstream := httptest.NewServer(http.HandlerFunc(func(response http.ResponseWriter, request *http.Request) {
		authorization <- request.Header.Get("Authorization")
		_, _ = io.WriteString(response, "ok")
	}))
	defer upstream.Close()
	target, _ := url.Parse(upstream.URL)
	gateway := httptest.NewServer(authenticatedProxy(target, "secret"))
	defer gateway.Close()

	request, _ := http.NewRequest(http.MethodGet, gateway.URL+"/v1/models", nil)
	request.Header.Set("Authorization", "Bearer secret")
	response, err := http.DefaultClient.Do(request)
	if err != nil {
		t.Fatal(err)
	}
	_ = response.Body.Close()

	if got := <-authorization; got != "" {
		t.Fatalf("Ollama received Authorization %q", got)
	}
}
