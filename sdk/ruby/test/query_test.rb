# frozen_string_literal: true

# What the builder has to guarantee, checked against the real core.
#
# Against the core rather than against the AST alone: the AST is only interesting
# because of what the core makes of it, and a test that asserted on hash shape
# would pass just as happily if the core rejected every query.

require "minitest/autorun"
require "pathname"

$LOAD_PATH.unshift(File.expand_path("../lib", __dir__))
require "thetabase"

class QueryTest < Minitest::Test
  def self.repo_root
    dir = Pathname.new(__dir__)
    dir = dir.parent until dir.join("sdk/conformance/cases.json").file? || dir.root?
    dir
  end

  CORE = ThetaBase::Scribe.load(
    repo_root.join("target/wasm32-unknown-unknown/wasm/theta_scribe_wasm.wasm")
  )

  def test_a_value_never_reaches_the_query_text
    # The property the whole builder exists for.
    hostile = "'; DROP TABLE users; --"
    rendered = ThetaBase::Query.table("users").where(ThetaBase::Query.eq("name", hostile)).render(CORE)

    assert_equal "SELECT * FROM users WHERE name = $p0", rendered["sql"]
    refute_includes rendered["sql"], "DROP"
    assert_equal hostile.to_json, rendered["params"]["p0"]
  end

  def test_repeated_filters_are_anded_rather_than_replaced
    rendered = ThetaBase::Query.table("users")
                             .where(ThetaBase::Query.eq("email", "a@example.com"))
                             .where(ThetaBase::Query.gt("age", 30))
                             .render(CORE)

    assert_includes rendered["sql"], " AND "
    assert_equal 2, rendered["params"].size
  end

  def test_a_builder_never_mutates_what_it_was_derived_from
    # In Ruby this is not hypothetical the way it is in Java: a hash and its
    # arrays are shared by reference until something copies them, so `select`
    # returning `@ast["columns"] << column` would append to the base query.
    base = ThetaBase::Query.table("users").select("id")
    left = base.select("email")
    right = base.select("name")

    assert_equal 1, base.ast["columns"].size
    assert_equal "SELECT id, email FROM users", left.render(CORE)["sql"]
    assert_equal "SELECT id, name FROM users", right.render(CORE)["sql"]
  end

  def test_an_identifier_that_could_carry_syntax_is_refused
    # Refused rather than escaped. Escaping would mean deciding what is safe to
    # quote, and the safe answer is that a name needing quotes is not a name this
    # subset accepts.
    assert_raises(ThetaBase::ProtocolError) do
      ThetaBase::Query.table("users; DROP TABLE x").render(CORE)
    end
  end

  def test_a_query_with_no_projection_selects_everything
    # The Go SDK shipped this broken: an empty slice can be nil there and
    # marshals to `null`, which the core reads as a missing sequence. Pinned in
    # every binding rather than only in the one that got it wrong.
    ast = ThetaBase::Query.table("users").ast

    assert_kind_of Array, ast["columns"]
    assert_kind_of Array, ast["orderBy"]
    assert_equal "SELECT * FROM users", ThetaBase::Query.table("users").render(CORE)["sql"]
  end

  def test_a_generated_type_round_trips_through_its_wire_names
    # The mapping is carried in a frozen WIRE hash rather than derived, because
    # deriving it would mean reversing snake_case and `writeVolumeMB` does not
    # survive that.
    diff = ThetaBase::Wire::ApplyRequest.new(change_id: "c1", retired_change: "", confirm: true)
    wire = diff.to_wire

    assert_equal "c1", wire["changeId"]
    assert_equal diff, ThetaBase::Wire::ApplyRequest.from_wire(wire)
  end
end
