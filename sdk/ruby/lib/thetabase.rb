# frozen_string_literal: true

# The ThetaBase Ruby SDK.
#
# Three pieces, the same three every ThetaBase SDK has: generated wire types, a
# typed query builder, and a transport binding onto the WebAssembly core.

require_relative "thetabase/dial"
require_relative "thetabase/generated"
require_relative "thetabase/query"
require_relative "thetabase/scribe"

module ThetaBase
  VERSION = "0.0.2"
end
