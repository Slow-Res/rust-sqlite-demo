# /api/members. Every endpoint requires a librarian.
class MembersController < ApplicationController
  before_action :require_user!

  def index
    page, per_page, after = query_uint32("page"), query_uint32("per_page"), query_string("after")
    q = query_string("q")
    page_params, cursor = Pagination.params(page, per_page, after, :string)

    members, total = Member.list(Pagination.search_term(q), page_params, cursor)
    render json: Pagination.envelope(page_params, members, total, cursor_key: :name.to_proc, &:as_api)
  end

  def show
    render json: find_member(path_id).as_api
  end

  def create
    member = member_from_body(json_body)
    member.validate_input!
    member.created_at = member.updated_at = Clock.now
    member.save!(validate: false)
    render json: Member.find_with_active_loans(member.id).as_api, status: :created
  end

  def update
    id = path_id
    candidate = member_from_body(json_body)
    candidate.validate_input!
    raise ApiError.not_found("member") unless Member.update_fields(id, candidate, Clock.now)

    render json: find_member(id).as_api
  end

  def destroy
    case Member.delete_checked(path_id)
    when :deleted then head :no_content
    when :not_found then raise ApiError.not_found("member")
    when :has_active_loans
      raise ApiError.conflict("member_has_active_loans", "member has active loans and cannot be deleted")
    end
  end

  private

  def find_member(id)
    Member.find_with_active_loans(id) || raise(ApiError.not_found("member"))
  end

  def member_from_body(body)
    Member.from_input(name: body_string(body, "name"), email: body_string(body, "email"))
  end
end
