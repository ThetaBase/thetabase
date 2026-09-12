package io.thetabase;

import java.io.FileInputStream;
import java.io.IOException;
import java.io.InputStream;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.KeyStore;
import java.security.cert.Certificate;
import java.security.cert.CertificateFactory;
import java.util.Collection;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLSocket;
import javax.net.ssl.SSLSocketFactory;
import javax.net.ssl.TrustManager;
import javax.net.ssl.TrustManagerFactory;

/**
 * Dialling {@code thetad}, with TLS where the address calls for it.
 *
 * <p>This class exists because of a gap, not a feature. The SDK took a {@link
 * Socket} from the caller and left the dialling to them, and every caller
 * reached for {@code new Socket(host, port)} — including the conformance
 * harness in this repository. A provisioned instance is at {@code
 * <app>.fly.dev:443}, where Fly's proxy terminates TLS, and a <em>shared</em>
 * IPv4 is routed by the SNI name in the handshake: a plaintext connection to
 * that port is not merely unencrypted, it cannot be routed to the right app at
 * all.
 *
 * <p>Leaving that to the caller means asking every user of this SDK to get SNI,
 * certificate verification and a private-CA escape hatch right on their own.
 * Nobody will, and the failure when they do not is a hang rather than an error.
 */
public final class Dial {
    private Dial() {}

    /** How long to wait for a connection before giving up. */
    private static final int CONNECT_TIMEOUT_MS = 30_000;

    /**
     * Open a socket to {@code thetad} at {@code address}, in {@code host:port}
     * form as {@code THETA_ADDRESS} carries it.
     *
     * <p>TLS is chosen from the address; see {@link #wantsTls(int)}.
     */
    public static Socket open(String address) throws IOException {
        // `lastIndexOf` rather than `indexOf`: an IPv6 address is full of
        // colons, and splitting on the first one yields nonsense.
        int colon = address.lastIndexOf(':');
        if (colon <= 0) {
            throw new IOException(
                    "`" + address + "` is not `host:port`. THETA_ADDRESS is set by"
                            + " `theta exec`; a hand-written value is usually a URL by"
                            + " mistake.");
        }
        String host = address.substring(0, colon).replace("[", "").replace("]", "");
        int port;
        try {
            port = Integer.parseInt(address.substring(colon + 1).trim());
        } catch (NumberFormatException e) {
            throw new IOException("`" + address + "` does not end in a port number");
        }

        if (!wantsTls(port)) {
            Socket socket = new Socket();
            socket.connect(new InetSocketAddress(host, port), CONNECT_TIMEOUT_MS);
            // Nagle off. Every exchange is one small frame and then a wait for
            // the answer, which is the exact shape Nagle delays.
            socket.setTcpNoDelay(true);
            return socket;
        }

        SSLSocketFactory factory = tlsContext().getSocketFactory();
        SSLSocket socket = (SSLSocket) factory.createSocket();
        socket.connect(new InetSocketAddress(host, port), CONNECT_TIMEOUT_MS);
        socket.setTcpNoDelay(true);
        socket.setEnabledProtocols(new String[] {"TLSv1.3", "TLSv1.2"});

        // `HTTPS` endpoint identification, which is what turns on hostname
        // verification. Without it a certificate from any trusted CA for any
        // host is accepted -- the chain is checked and the *name* is not, which
        // is the default in the JSSE and the single most commonly missed line in
        // Java TLS code.
        javax.net.ssl.SSLParameters params = socket.getSSLParameters();
        params.setEndpointIdentificationAlgorithm("HTTPS");
        // The SNI name, and on a shared address it is what the proxy routes on
        // -- so getting it wrong does not produce a certificate error, it
        // produces a connection to the wrong instance or to none.
        params.setServerNames(
                java.util.List.of(new javax.net.ssl.SNIHostName(host)));
        socket.setSSLParameters(params);

        try {
            socket.startHandshake();
        } catch (IOException e) {
            socket.close();
            throw new IOException(
                    "the TLS handshake with `" + host + "` failed: " + e.getMessage()
                            + ". A provisioned instance presents a publicly signed"
                            + " certificate; a self-hosted one needs its CA in"
                            + " THETA_TLS_CA.",
                    e);
        }
        return socket;
    }

    /**
     * Whether a connection to this port should be dialled with TLS.
     *
     * <p>{@code THETA_TLS} wins when it is set, so a self-hosted instance behind
     * a terminating proxy on some other port can say so — and so can a developer
     * tunnelling 443 to a local plaintext process.
     *
     * <p>Otherwise: port 443 means TLS. That is the port Fly's proxy listens on
     * and the port the Control Plane hands out, and it is the only port in this
     * product's vocabulary that implies a terminator in front of {@code thetad}.
     * Local instances are on 7700 and upwards.
     *
     * <p>Inferred from the address rather than carried beside it, deliberately,
     * and identically to every other client. The address travels through {@code
     * THETA_ADDRESS} into every SDK; a second variable that had to agree with it
     * would be a second thing to get wrong, and the symptom of getting it wrong
     * is a <em>hang</em> rather than an error — a plaintext frame sent to a TLS
     * listener is read as a ClientHello, found malformed, and discarded.
     */
    public static boolean wantsTls(int port) {
        String over = System.getenv("THETA_TLS");
        if (over != null) {
            switch (over.toLowerCase()) {
                case "1":
                case "true":
                case "require":
                case "yes":
                    return true;
                case "0":
                case "false":
                case "off":
                case "no":
                    return false;
                default:
                    break;
            }
        }
        return port == 443;
    }

    /**
     * A verifying TLS context, with a deployment's own CA if it named one.
     *
     * <p>{@code THETA_TLS_CA} adds to the platform roots rather than replacing
     * them. A customer running {@code thetad} behind their own terminating proxy
     * signs with their own CA, and a client that trusts only the public set
     * cannot reach it — and "turn TLS off instead" is not an answer for a
     * database.
     *
     * <p>There is deliberately no way to disable verification. A flag that turns
     * off certificate checking is a flag that ends up set in production, and the
     * whole reason this transport exists is that a session token was about to
     * cross the open internet.
     */
    private static SSLContext tlsContext() throws IOException {
        String extra = System.getenv("THETA_TLS_CA");
        if (extra == null || extra.isEmpty()) {
            try {
                return SSLContext.getDefault();
            } catch (Exception e) {
                throw new IOException("no usable TLS provider: " + e.getMessage(), e);
            }
        }

        // A refusal, not a warning. An operator who set this has said "trust
        // this CA"; continuing without it would silently connect under a laxer
        // trust policy than the one they chose.
        Path path = Path.of(extra);
        if (!Files.isReadable(path)) {
            throw new IOException(
                    "THETA_TLS_CA points at `" + extra + "`, which cannot be read");
        }

        try {
            // Started from the platform store rather than an empty one, so
            // naming a private CA does not stop this process reaching a
            // provisioned instance.
            KeyStore store = platformTrustStore();
            CertificateFactory certificates = CertificateFactory.getInstance("X.509");
            int added = 0;
            try (InputStream in = new FileInputStream(path.toFile())) {
                Collection<? extends Certificate> loaded =
                        certificates.generateCertificates(in);
                for (Certificate certificate : loaded) {
                    store.setCertificateEntry("theta-tls-ca-" + added, certificate);
                    added++;
                }
            }
            if (added == 0) {
                throw new IOException(
                        "THETA_TLS_CA `" + extra + "` contains no certificates. An"
                                + " empty trust file is almost certainly the wrong"
                                + " file, and ignoring it would mean connecting under"
                                + " a trust policy nobody chose.");
            }

            TrustManagerFactory managers =
                    TrustManagerFactory.getInstance(
                            TrustManagerFactory.getDefaultAlgorithm());
            managers.init(store);

            SSLContext context = SSLContext.getInstance("TLS");
            context.init(null, managers.getTrustManagers(), null);
            return context;
        } catch (IOException e) {
            throw e;
        } catch (Exception e) {
            throw new IOException(
                    "THETA_TLS_CA `" + extra + "` could not be loaded: " + e.getMessage(),
                    e);
        }
    }

    /**
     * The platform's trust store, as a keystore this code can add to.
     *
     * <p>Loaded by initialising a default {@link TrustManagerFactory} and copying
     * out what it trusts. There is no public API that hands over the default
     * store directly, and the alternative — reading {@code cacerts} off disk by
     * path — breaks on every JVM layout that is not the one it was written for.
     */
    private static KeyStore platformTrustStore() throws Exception {
        KeyStore store = KeyStore.getInstance(KeyStore.getDefaultType());
        store.load(null, null);

        TrustManagerFactory defaults =
                TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
        defaults.init((KeyStore) null);
        int index = 0;
        for (TrustManager manager : defaults.getTrustManagers()) {
            if (!(manager instanceof javax.net.ssl.X509TrustManager x509)) {
                continue;
            }
            for (java.security.cert.X509Certificate root : x509.getAcceptedIssuers()) {
                store.setCertificateEntry("platform-root-" + index, root);
                index++;
            }
        }
        return store;
    }
}
