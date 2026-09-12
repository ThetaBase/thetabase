# frozen_string_literal: true

# Dialling `thetad`, with TLS where the address calls for it.
#
# This file exists because of a gap, not a feature. The SDK took a socket from
# the caller and left the dialling to them, and every caller reached for
# `TCPSocket.new` -- including the conformance harness in this repository. A
# provisioned instance is at `<app>.fly.dev:443`, where Fly's proxy terminates
# TLS, and a *shared* IPv4 is routed by the SNI name in the handshake: a
# plaintext connection to that port is not merely unencrypted, it cannot be
# routed to the right app at all.
#
# Leaving that to the caller means asking every user of this SDK to get SNI,
# certificate verification and a private-CA escape hatch right on their own.
# Nobody will, and the failure when they do not is a hang rather than an error.

require "openssl"
require "socket"

module ThetaBase
  # Open a socket to `thetad` at +address+, in `host:port` form as
  # `THETA_ADDRESS` carries it.
  #
  # TLS is chosen from the address; see {ThetaBase.wants_tls?}.
  def self.dial(address)
    host, _, port = address.rpartition(":")
    # `rpartition` rather than `split`: an IPv6 address is full of colons, and
    # splitting on the first yields nonsense.
    raise ArgumentError, "`#{address}` is not `host:port`" if host.empty? || port.empty?

    host = host.delete_prefix("[").delete_suffix("]")
    socket = TCPSocket.new(host, port.to_i)

    # Nagle off. Every exchange is one small frame and then a wait for the
    # answer, which is the exact shape Nagle delays.
    socket.setsockopt(Socket::IPPROTO_TCP, Socket::TCP_NODELAY, 1)

    return socket unless wants_tls?(port)

    secure = OpenSSL::SSL::SSLSocket.new(socket, tls_context)
    # The SNI name, and on a shared address it is what the proxy routes on --
    # so getting it wrong does not produce a certificate error, it produces a
    # connection to the wrong instance or to none.
    secure.hostname = host
    begin
      secure.connect
      # `connect` verifies the chain; this verifies the *name*, which it does
      # not. Without it a certificate from any trusted CA for any host would be
      # accepted.
      secure.post_connection_check(host)
    rescue OpenSSL::SSL::SSLError => e
      socket.close
      raise OpenSSL::SSL::SSLError,
            "the TLS handshake with `#{host}` failed: #{e.message}. A provisioned " \
            "instance presents a publicly signed certificate; a self-hosted one " \
            "needs its CA in THETA_TLS_CA."
    end
    secure
  end

  # Whether a connection to this port should be dialled with TLS.
  #
  # `THETA_TLS` wins when it is set, so a self-hosted instance behind a
  # terminating proxy on some other port can say so -- and so can a developer
  # tunnelling 443 to a local plaintext process.
  #
  # Otherwise: port 443 means TLS. That is the port Fly's proxy listens on and
  # the port the Control Plane hands out, and it is the only port in this
  # product's vocabulary that implies a terminator in front of `thetad`. Local
  # instances are on 7700 and upwards.
  #
  # Inferred from the address rather than carried beside it, deliberately, and
  # identically to every other client. The address travels through
  # `THETA_ADDRESS` into every SDK; a second variable that had to agree with it
  # would be a second thing to get wrong, and the symptom of getting it wrong is
  # a *hang* rather than an error -- a plaintext frame sent to a TLS listener is
  # read as a ClientHello, found malformed, and discarded.
  def self.wants_tls?(port)
    case ENV.fetch("THETA_TLS", "").downcase
    when "1", "true", "require", "yes" then return true
    when "0", "false", "off", "no" then return false
    end
    port.to_s.strip == "443"
  end

  # A verifying TLS context, with a deployment's own CA if it named one.
  #
  # `THETA_TLS_CA` adds to the default roots rather than replacing them. A
  # customer running `thetad` behind their own terminating proxy signs with
  # their own CA, and a client that trusts only the public set cannot reach it
  # -- and "turn TLS off instead" is not an answer for a database.
  #
  # There is deliberately no way to disable verification. A flag that turns off
  # certificate checking is a flag that ends up set in production, and the whole
  # reason this transport exists is that a session token was about to cross the
  # open internet.
  def self.tls_context
    context = OpenSSL::SSL::SSLContext.new
    context.verify_mode = OpenSSL::SSL::VERIFY_PEER
    context.min_version = OpenSSL::SSL::TLS1_2_VERSION
    context.cert_store = OpenSSL::X509::Store.new
    context.cert_store.set_default_paths

    ca = ENV.fetch("THETA_TLS_CA", "")
    unless ca.empty?
      # A refusal, not a warning. An operator who set this has said "trust this
      # CA"; continuing without it would silently connect under a laxer trust
      # policy than the one they chose.
      unless File.readable?(ca)
        raise ArgumentError,
              "THETA_TLS_CA points at `#{ca}`, which cannot be read"
      end
      context.cert_store.add_file(ca)
    end

    context
  end
end
