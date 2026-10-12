class ApplicationRecord < ActiveRecord::Base
  primary_abstract_class

  # Timestamps are Unix seconds in BIGINT columns, set by hand from Clock.
  self.record_timestamps = false

  # Validation messages by field name, the first one of each field, in name order.
  def field_errors
    errors.to_hash.to_h { |field, messages| [field.to_s, messages.first] }.sort.to_h
  end

  # Raise the 422 of the API unless the record is valid.
  def validate_input!
    raise ApiError.validation(field_errors) unless valid?
  end
end
