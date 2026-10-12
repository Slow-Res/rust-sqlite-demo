# /api/auth: login, logout, me.
class AuthController < ApplicationController
  before_action :require_user!, only: %i[logout me]

  def login
    body = json_body
    username = body_string(body, "username")
    password = body_string(body, "password")
    errors = {}
    errors["username"] = "must not be empty" if Text.trim(username).empty?
    errors["password"] = "must not be empty" if password.empty?
    raise ApiError.validation(errors) if errors.any?

    session = Authentication.login(Text.trim(username), password)
    response.set_cookie(SESSION_COOKIE, value: session.token, path: "/", httponly: true, same_site: :lax,
                                        secure: Settings::COOKIE_SECURE, max_age: Settings::SESSION_TTL_SECONDS)
    render json: { id: session.id, username: session.username }
  end

  def logout
    # `require_user!` already proved the cookie exists and is valid.
    Authentication.logout(request.cookies[SESSION_COOKIE])
    response.delete_cookie(SESSION_COOKIE, path: "/")
    head :no_content
  end

  def me
    id, username = @current_user
    render json: { id: id, username: username }
  end
end
