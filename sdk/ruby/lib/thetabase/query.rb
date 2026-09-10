# frozen_string_literal: true

# The typed query builder.
#
# Chainable in Ruby, because that is what makes a query pleasant to write. What
# it produces is an AST, and the AST is rendered to SQL-subset source and bound
# parameters by the WebAssembly core — so the same query built in TypeScript,
# Python, Go, Rust, Java or C# reaches the server as the same bytes, and the rule
# that a value never becomes query text has one implementation rather than one
# per language.
#
# Nothing here interpolates. `where(ThetaBase::Query.eq("name", user_input))` puts
# `user_input` in the parameter map and `$p0` in the text, whatever it contains.

module ThetaBase
  # A typed plan under construction.
  class Query
    # The comparisons the renderer knows. Closed on purpose: an open set would
    # eventually carry an operator the renderer does not know, and the only place
    # left to put it would be the query text.
    OPS = %w[eq ne lt lte gt gte].freeze

    class << self
      OPS.each do |op|
        define_method(op) do |column, value|
          { "kind" => "compare", "column" => column, "op" => op, "value" => value }
        end
      end

      # Keep rows where +column+ is one of +values+.
      def in_(column, values)
        { "kind" => "in", "column" => column, "values" => values }
      end

      # Keep rows where +column+ is null.
      def is_null(column)
        { "kind" => "isNull", "column" => column }
      end

      # Keep rows where +column+ is not null.
      def not_null(column)
        { "kind" => "notNull", "column" => column }
      end

      # Keep rows matching every term.
      def and_(*terms)
        { "kind" => "and", "terms" => terms }
      end

      # Keep rows matching any term.
      def or_(*terms)
        { "kind" => "or", "terms" => terms }
      end

      # Invert a term.
      def not_(term)
        { "kind" => "not", "term" => term }
      end

      # Start a query against +name+.
      def table(name)
        new({ "table" => name, "columns" => [], "orderBy" => [] })
      end
    end

    def initialize(ast)
      @ast = ast.freeze
    end

    # Add columns to the projection.
    #
    # Every method here returns a new Query rather than mutating this one. A
    # builder that mutated would make a shared base query change under whoever
    # else was holding it — and in Ruby that is not hypothetical, because a hash
    # and its arrays are shared by reference until something copies them.
    def select(*columns)
      with("columns" => @ast["columns"] + columns)
    end

    # Keep rows matching +predicate+. Repeated calls are ANDed.
    def where(predicate)
      filter = @ast["filter"] ? Query.and_(@ast["filter"], predicate) : predicate
      with("filter" => filter)
    end

    # Add a sort key.
    def order_by(column, descending = false)
      with("orderBy" => @ast["orderBy"] + [{ "column" => column, "descending" => descending }])
    end

    # Cap the number of rows returned.
    def limit(count)
      with("limit" => count)
    end

    # Skip rows before returning any.
    def offset(count)
      with("offset" => count)
    end

    # The plan, for a caller that wants to inspect or send it.
    def ast
      Marshal.load(Marshal.dump(@ast))
    end

    # Render through the core. Raises if an identifier could carry syntax.
    def render(core)
      core.render_query(@ast)
    end

    private

    def with(changes)
      Query.new(@ast.merge(changes))
    end
  end
end
