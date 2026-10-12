# The same routes as apps/web (books, members, loans, auth, health), all under /api.
Rails.application.routes.draw do
  match "/api", to: "errors#not_found", via: :all, format: false

  scope "/api", format: false do
    get "health", to: "health#show"

    post "auth/login", to: "auth#login"
    post "auth/logout", to: "auth#logout"
    get "auth/me", to: "auth#me"

    # Any segment is an id here: a non-numeric one is a 400 from the controller,
    # as in axum's `Path<i64>`, not an unmatched route.
    constraints(id: %r{[^/]+}) do
      get "books", to: "books#index"
      post "books", to: "books#create"
      get "books/:id", to: "books#show"
      put "books/:id", to: "books#update"
      delete "books/:id", to: "books#destroy"

      get "members", to: "members#index"
      post "members", to: "members#create"
      get "members/:id", to: "members#show"
      put "members/:id", to: "members#update"
      delete "members/:id", to: "members#destroy"

      get "loans", to: "loans#index"
      post "loans", to: "loans#create"
      post "loans/:id/return", to: "loans#return_loan"

      # A known path with another method is a 405 (no PATCH either), an unknown one a 404.
      {
        "health" => "GET, HEAD", "auth/login" => "POST", "auth/logout" => "POST", "auth/me" => "GET, HEAD",
        "books" => "GET, HEAD, POST", "books/:id" => "GET, HEAD, PUT, DELETE",
        "members" => "GET, HEAD, POST", "members/:id" => "GET, HEAD, PUT, DELETE",
        "loans" => "GET, HEAD, POST", "loans/:id/return" => "POST"
      }.each do |path, allow|
        match path, to: "errors#method_not_allowed", via: :all, defaults: { allow: allow }
      end
      match "*path", to: "errors#not_found", via: :all
    end
  end
end
