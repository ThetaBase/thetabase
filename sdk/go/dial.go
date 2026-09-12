// Dialling `thetad`, with TLS where the address calls for it.
//
// This file exists because of a gap, not a feature. The SDK exposes a `Socket`
// interface and left the dialling to the caller, and every caller reached for
// `net.Dial("tcp", address)` -- including the conformance harness in this
// repository. A provisioned instance is at `<app>.fly.dev:443`, where Fly's
// proxy terminates TLS, and a *shared* IPv4 is routed by the SNI name in the
// handshake: a plaintext connection to that port is not merely unencrypted, it
// cannot be routed to the right app at all.
//
// Leaving that to the caller means asking every user of this SDK to get SNI,
// certificate verification and a private-CA escape hatch right on their own.
// Nobody will, and the failure when they do not is a hang rather than an error.

package thetabase

import (
	"crypto/tls"
	"crypto/x509"
	"fmt"
	"net"
	"os"
	"strings"
	"time"
)

// Dial opens a framed connection to `thetad` at `address`, in `host:port` form
// as `THETA_ADDRESS` carries it.
//
// TLS is chosen from the address; see [WantsTLS].
func Dial(address string) (Socket, error) {
	host, port, err := net.SplitHostPort(address)
	if err != nil {
		// `SplitHostPort` rather than splitting on a colon: an IPv6 address is
		// full of them, and splitting on the first yields nonsense.
		return nil, fmt.Errorf(
			"`%s` is not `host:port`. THETA_ADDRESS is set by `theta exec`; a "+
				"hand-written value is usually a URL by mistake: %w", address, err)
	}

	conn, err := net.DialTimeout("tcp", address, 30*time.Second)
	if err != nil {
		return nil, fmt.Errorf("cannot reach the instance at %s: %w", address, err)
	}

	// Nagle off. Every exchange is one small frame and then a wait for the
	// answer, which is the exact shape Nagle delays.
	if tcp, ok := conn.(*net.TCPConn); ok {
		_ = tcp.SetNoDelay(true)
	}

	if !WantsTLS(port) {
		return &netSocket{conn: conn}, nil
	}

	roots, err := trustRoots()
	if err != nil {
		conn.Close()
		return nil, err
	}

	// ServerName is the SNI name, and on a shared address it is what the proxy
	// routes on -- so getting it wrong does not produce a certificate error, it
	// produces a connection to the wrong instance or to none.
	//
	// There is deliberately no InsecureSkipVerify option anywhere in this SDK.
	// A flag that turns off certificate checking is a flag that ends up set in
	// production, and the whole reason this transport exists is that a session
	// token was about to cross the open internet.
	secure := tls.Client(conn, &tls.Config{
		ServerName: host,
		RootCAs:    roots,
		MinVersion: tls.VersionTLS12,
	})
	if err := secure.Handshake(); err != nil {
		conn.Close()
		return nil, fmt.Errorf(
			"the TLS handshake with `%s` failed: %w. A provisioned instance "+
				"presents a publicly signed certificate; a self-hosted one needs "+
				"its CA in THETA_TLS_CA", host, err)
	}

	return &netSocket{conn: secure}, nil
}

// WantsTLS reports whether a connection to this port should be dialled with
// TLS.
//
// `THETA_TLS` wins when it is set, so a self-hosted instance behind a
// terminating proxy on some other port can say so -- and so can a developer
// tunnelling 443 to a local plaintext process.
//
// Otherwise: port 443 means TLS. That is the port Fly's proxy listens on and
// the port the Control Plane hands out, and it is the only port in this
// product's vocabulary that implies a terminator in front of `thetad`. Local
// instances are on 7700 and upwards.
//
// Inferred from the address rather than carried beside it, deliberately, and
// identically to the Rust, Python, TypeScript and C# clients. The address
// travels through `THETA_ADDRESS` into every SDK; a second variable that had to
// agree with it would be a second thing to get wrong, and the symptom of
// getting it wrong is a *hang* rather than an error -- a plaintext frame sent to
// a TLS listener is read as a ClientHello, found malformed, and discarded.
func WantsTLS(port string) bool {
	switch strings.ToLower(os.Getenv("THETA_TLS")) {
	case "1", "true", "require", "yes":
		return true
	case "0", "false", "off", "no":
		return false
	}
	return strings.TrimSpace(port) == "443"
}

// trustRoots returns the platform roots, plus a deployment's own CA if
// `THETA_TLS_CA` names one.
//
// The extra CA widens what the client will accept; it does not pin. A customer
// running `thetad` behind their own terminating proxy signs with their own CA,
// and a client that trusts only the public set cannot reach it -- and "turn TLS
// off instead" is not an answer for a database.
//
// A path that cannot be read is a refusal, not a warning. An operator who set
// this has said "trust this CA", and continuing without it would silently
// connect under a laxer trust policy than the one they chose.
func trustRoots() (*x509.CertPool, error) {
	path := os.Getenv("THETA_TLS_CA")
	if path == "" {
		// nil means "the platform roots", which is what a provisioned instance
		// needs.
		return nil, nil
	}

	pem, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf(
			"THETA_TLS_CA points at `%s`, which cannot be read: %w", path, err)
	}

	// Starting from the platform set rather than an empty pool, so naming a
	// private CA does not stop the client reaching a provisioned instance in
	// the same process.
	pool, err := x509.SystemCertPool()
	if err != nil {
		pool = x509.NewCertPool()
	}
	if !pool.AppendCertsFromPEM(pem) {
		return nil, fmt.Errorf(
			"THETA_TLS_CA `%s` contains no certificates. An empty trust file is "+
				"almost certainly the wrong file, and ignoring it would mean "+
				"connecting under a trust policy nobody chose", path)
	}
	return pool, nil
}

// netSocket adapts a `net.Conn` to [Socket].
//
// Both a plain TCP connection and a TLS one satisfy `net.Conn`, which is why
// the transport choice lives in `Dial` and nowhere else.
type netSocket struct {
	conn net.Conn
}

func (s *netSocket) Write(bytes []byte) error {
	_, err := s.conn.Write(bytes)
	return err
}

// ReadFull returns exactly n bytes, or an error if the peer closes first.
//
// A connection delivers whatever arrived, not what was asked for, and a short
// read here would frame the next message wrong.
func (s *netSocket) ReadFull(n int) ([]byte, error) {
	buffer := make([]byte, n)
	read := 0
	for read < n {
		got, err := s.conn.Read(buffer[read:])
		if err != nil {
			return nil, err
		}
		if got == 0 {
			return nil, fmt.Errorf("the server closed the connection")
		}
		read += got
	}
	return buffer, nil
}

func (s *netSocket) Close() error { return s.conn.Close() }
